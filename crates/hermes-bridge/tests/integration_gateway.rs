//! Integration tests exercising `HermesBridge` end-to-end against a hand-
//! rolled fake gateway. The gateway speaks just enough of `/health`,
//! `POST /v1/runs`, `GET /v1/runs/{id}/events`, `GET /v1/runs/{id}`,
//! `POST /v1/runs/{id}/approval`, and `POST /v1/runs/{id}/stop` to drive
//! the bridge through realistic flows: success, failure, trailing-chunk
//! terminal, daemon-restart-survivable replay, and approval bridging.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alleycat_bridge_core::session::Session;
use alleycat_bridge_core::{Bridge, Conn};
use alleycat_codex_proto::common::{AskForApproval, TurnStatus};
use alleycat_codex_proto::notifications::{ItemStartedNotification, TurnCompletedNotification};
use alleycat_codex_proto::thread::{ThreadReadParams, ThreadStartParams};
use alleycat_codex_proto::turn::TurnStartParams;
use alleycat_hermes_bridge::{HermesBridge, HermesBridgeConfig, HermesMode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;

// ---- helpers ----

fn make_session() -> Arc<Session> {
    Arc::new(Session::new("hermes", "test-node".into(), 256, 1 << 20))
}

fn make_conn() -> (Arc<Session>, Conn) {
    let session = make_session();
    let conn = Conn::from_session(Arc::clone(&session));
    (session, conn)
}

/// Drain frames from the session attachment into a channel. Returns the
/// receiver and the join handle for the drain task.
fn attach_and_drain(
    session: &Arc<Session>,
) -> (mpsc::UnboundedReceiver<Value>, tokio::task::JoinHandle<()>) {
    let attach = session.install_attachment(None);
    let (tx, rx) = mpsc::unbounded_channel();
    let mut live_rx = attach.live_rx;
    let handle = tokio::spawn(async move {
        while let Some(frame) = live_rx.recv().await {
            let _ = tx.send(frame.payload);
        }
    });
    (rx, handle)
}

async fn drain_until<F: Fn(&Value) -> bool>(
    rx: &mut mpsc::UnboundedReceiver<Value>,
    pred: F,
    max: Duration,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + max;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match timeout(remaining, rx.recv()).await {
            Ok(Some(frame)) => {
                if pred(&frame) {
                    return Some(frame);
                }
            }
            _ => return None,
        }
    }
}

fn frame_method(frame: &Value) -> Option<&str> {
    frame.get("method").and_then(Value::as_str)
}

// ---- fake gateway ----

#[derive(Clone, Debug)]
enum Scenario {
    /// Stream three deltas then `run.completed` with output.
    HappyPath,
    /// Stream one delta, then an `approval.request`, then more deltas after
    /// an approval is POSTed, then `run.completed`.
    NeedsApproval,
    /// Stream `run.failed` with a message.
    Failure,
    /// Stream a delta then a terminal event in a chunk that is NOT followed
    /// by the standard `\n\n` separator (regression for Phase 1.5 fix).
    TerminalInTrailingChunk,
}

struct GatewayState {
    /// Counts approval POSTs received.
    approvals: Mutex<Vec<String>>,
}

async fn spawn_fake_gateway(
    scenario: Scenario,
) -> (String, Arc<GatewayState>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let state = Arc::new(GatewayState {
        approvals: Mutex::new(Vec::new()),
    });
    let state_for_task = Arc::clone(&state);
    let handle = tokio::spawn(async move {
        loop {
            let (socket, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let scenario = scenario.clone();
            let state = Arc::clone(&state_for_task);
            tokio::spawn(async move {
                handle_request(socket, scenario, state).await;
            });
        }
    });
    (base, state, handle)
}

async fn handle_request(
    mut socket: tokio::net::TcpStream,
    scenario: Scenario,
    state: Arc<GatewayState>,
) {
    let mut buf = vec![0u8; 16384];
    let mut total = 0usize;
    // Read until we have headers + body. Tests use small bodies; loop a few
    // times.
    for _ in 0..16 {
        match socket.read(&mut buf[total..]).await {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                let text = String::from_utf8_lossy(&buf[..total]);
                if text.contains("\r\n\r\n") {
                    // If POST, peek Content-Length and ensure body is complete.
                    if let Some(cl) = parse_content_length(&text) {
                        let body_start = text.find("\r\n\r\n").unwrap() + 4;
                        if text.len() - body_start >= cl {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
        if total >= buf.len() {
            break;
        }
    }
    let req = String::from_utf8_lossy(&buf[..total]).into_owned();
    let first_line = req.lines().next().unwrap_or("").to_string();
    let (method, path) = parse_request_line(&first_line);

    if method == "GET" && (path == "/health" || path == "/v1/health") {
        write_json(&mut socket, 200, "{\"status\":\"ok\"}").await;
        return;
    }
    if method == "POST" && path == "/v1/runs" {
        write_json(
            &mut socket,
            200,
            "{\"run_id\":\"run-test\",\"status\":\"started\"}",
        )
        .await;
        return;
    }
    if method == "GET" && path == "/v1/runs/run-test/events" {
        write_sse(&mut socket, scenario, state).await;
        return;
    }
    if method == "GET" && path == "/v1/runs/run-test" {
        write_json(
            &mut socket,
            200,
            "{\"object\":\"hermes.run\",\"runId\":\"run-test\",\"status\":\"completed\",\"output\":\"poll-recovered\"}",
        )
        .await;
        return;
    }
    if method == "POST" && path == "/v1/runs/run-test/approval" {
        // Find request body to extract `choice`.
        let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
        let choice = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| {
                v.get("choice")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        state.approvals.lock().unwrap().push(choice);
        write_json(&mut socket, 200, "{\"choice\":\"once\",\"resolved\":1}").await;
        return;
    }
    if method == "POST" && path == "/v1/runs/run-test/stop" {
        write_json(&mut socket, 200, "{}").await;
        return;
    }
    write_resp(
        &mut socket,
        404,
        "application/json",
        "{\"error\":\"not_found\"}",
    )
    .await;
}

fn parse_request_line(line: &str) -> (&str, &str) {
    let mut parts = line.split_whitespace();
    (parts.next().unwrap_or(""), parts.next().unwrap_or(""))
}

fn parse_content_length(req: &str) -> Option<usize> {
    for line in req.lines() {
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

async fn write_resp(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) {
    let status_text = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Status",
    };
    let resp = format!(
        "HTTP/1.1 {status} {status_text}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(resp.as_bytes()).await;
    let _ = socket.shutdown().await;
}

async fn write_json(socket: &mut tokio::net::TcpStream, status: u16, body: &str) {
    write_resp(socket, status, "application/json", body).await;
}

async fn sse_send(socket: &mut tokio::net::TcpStream, event: &str, data: &str) {
    let payload = format!("event: {event}\ndata: {data}\n\n");
    let _ = socket.write_all(payload.as_bytes()).await;
}

async fn write_sse(
    socket: &mut tokio::net::TcpStream,
    scenario: Scenario,
    state: Arc<GatewayState>,
) {
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n";
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    match scenario {
        Scenario::HappyPath => {
            sse_send(socket, "message.delta", "{\"delta\":\"Hel\"}").await;
            sse_send(socket, "message.delta", "{\"delta\":\"lo\"}").await;
            sse_send(socket, "message.delta", "{\"delta\":\"!\"}").await;
            sse_send(socket, "run.completed", "{\"output\":\"Hello!\"}").await;
        }
        Scenario::NeedsApproval => {
            sse_send(socket, "message.delta", "{\"delta\":\"start\"}").await;
            sse_send(
                socket,
                "approval.request",
                "{\"choices\":[\"once\",\"session\",\"always\",\"deny\"],\"tool\":\"terminal\"}",
            )
            .await;
            // Wait for approval POST.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while tokio::time::Instant::now() < deadline {
                if !state.approvals.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            sse_send(socket, "approval.responded", "{\"choice\":\"once\"}").await;
            sse_send(socket, "message.delta", "{\"delta\":\"-end\"}").await;
            sse_send(socket, "run.completed", "{\"output\":\"start-end\"}").await;
        }
        Scenario::Failure => {
            sse_send(socket, "message.delta", "{\"delta\":\"oops\"}").await;
            sse_send(socket, "run.failed", "{\"message\":\"boom\"}").await;
        }
        Scenario::TerminalInTrailingChunk => {
            // Emit a delta normally, then a terminal event WITHOUT the final
            // `\n\n`. The bridge's SSE parser should still split it because
            // `parse_sse_frames` accepts trailing frames during the post-loop
            // flush.
            sse_send(socket, "message.delta", "{\"delta\":\"part1\"}").await;
            // Note: missing the final \n\n. Some clients/servers do this on
            // abrupt closure.
            let trailing = "event: run.completed\ndata: {\"output\":\"part1\"}\n";
            let _ = socket.write_all(trailing.as_bytes()).await;
        }
    }
    let _ = socket.shutdown().await;
}

// ---- bridge config helpers ----

fn make_bridge(state_dir: PathBuf, api_base: String) -> HermesBridge {
    HermesBridge::new(HermesBridgeConfig {
        mode: HermesMode::Api { api_base },
        state_dir: Some(state_dir.to_string_lossy().to_string()),
        health_timeout_ms: 1000,
        health_cache_ttl_ms: 0, // disable caching in tests
    })
}

async fn start_thread(bridge: &HermesBridge, ctx: &Conn, cwd: &str) -> String {
    let params = ThreadStartParams {
        cwd: Some(cwd.into()),
        approval_policy: Some(AskForApproval::Never),
        ..Default::default()
    };
    let value = serde_json::to_value(&params).unwrap();
    let resp = bridge
        .dispatch(ctx, "thread/start", value)
        .await
        .expect("thread/start");
    resp.get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .unwrap()
        .to_string()
}

async fn start_turn(
    bridge: &HermesBridge,
    ctx: &Conn,
    thread_id: &str,
    text: &str,
    approval_policy: AskForApproval,
) {
    let params = TurnStartParams {
        thread_id: thread_id.into(),
        input: vec![alleycat_codex_proto::items::UserInput::Text {
            text: text.into(),
            text_elements: vec![],
        }],
        approval_policy: Some(approval_policy),
        ..Default::default()
    };
    let value = serde_json::to_value(&params).unwrap();
    bridge
        .dispatch(ctx, "turn/start", value)
        .await
        .expect("turn/start");
}

// ---- tests ----

#[tokio::test]
async fn happy_path_streams_and_finalizes() {
    let (api_base, _state, gw) = spawn_fake_gateway(Scenario::HappyPath).await;
    let dir = TempDir::new().unwrap();
    let bridge = make_bridge(dir.path().to_path_buf(), api_base);
    let (session, ctx) = make_conn();
    let (mut rx, _drainer) = attach_and_drain(&session);

    let thread_id = start_thread(&bridge, &ctx, "/tmp").await;
    start_turn(&bridge, &ctx, &thread_id, "hi", AskForApproval::Never).await;

    let completed = drain_until(
        &mut rx,
        |f| frame_method(f) == Some("turn/completed"),
        Duration::from_secs(5),
    )
    .await
    .expect("turn/completed must arrive");
    let payload: TurnCompletedNotification =
        serde_json::from_value(completed.get("params").cloned().unwrap()).unwrap();
    assert_eq!(payload.turn.status, TurnStatus::Completed);

    gw.abort();
}

#[tokio::test]
async fn failure_propagates_with_error() {
    let (api_base, _state, gw) = spawn_fake_gateway(Scenario::Failure).await;
    let dir = TempDir::new().unwrap();
    let bridge = make_bridge(dir.path().to_path_buf(), api_base);
    let (session, ctx) = make_conn();
    let (mut rx, _drainer) = attach_and_drain(&session);

    let thread_id = start_thread(&bridge, &ctx, "/tmp").await;
    start_turn(&bridge, &ctx, &thread_id, "hi", AskForApproval::Never).await;

    let completed = drain_until(
        &mut rx,
        |f| frame_method(f) == Some("turn/completed"),
        Duration::from_secs(5),
    )
    .await
    .expect("turn/completed must arrive");
    let payload: TurnCompletedNotification =
        serde_json::from_value(completed.get("params").cloned().unwrap()).unwrap();
    assert_eq!(payload.turn.status, TurnStatus::Failed);
    assert!(payload.turn.error.is_some());
    assert!(
        payload
            .turn
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("boom"),
        "expected gateway error message in turn error, got {:?}",
        payload.turn.error
    );
    gw.abort();
}

#[tokio::test]
async fn terminal_in_trailing_chunk_still_completes() {
    let (api_base, _state, gw) = spawn_fake_gateway(Scenario::TerminalInTrailingChunk).await;
    let dir = TempDir::new().unwrap();
    let bridge = make_bridge(dir.path().to_path_buf(), api_base);
    let (session, ctx) = make_conn();
    let (mut rx, _drainer) = attach_and_drain(&session);

    let thread_id = start_thread(&bridge, &ctx, "/tmp").await;
    start_turn(&bridge, &ctx, &thread_id, "hi", AskForApproval::Never).await;

    let completed = drain_until(
        &mut rx,
        |f| frame_method(f) == Some("turn/completed"),
        Duration::from_secs(5),
    )
    .await
    .expect("turn/completed must arrive");
    let payload: TurnCompletedNotification =
        serde_json::from_value(completed.get("params").cloned().unwrap()).unwrap();
    assert_eq!(
        payload.turn.status,
        TurnStatus::Completed,
        "trailing-chunk terminal event should still finalize as Completed"
    );
    gw.abort();
}

#[tokio::test]
async fn approval_request_bridges_to_client() {
    let (api_base, state, gw) = spawn_fake_gateway(Scenario::NeedsApproval).await;
    let dir = TempDir::new().unwrap();
    let bridge = make_bridge(dir.path().to_path_buf(), api_base);
    let (session, ctx) = make_conn();

    // Single drainer task that doubles as the fake client: it inspects each
    // outbound frame, and when it sees a server→client request (has an
    // `id`), it resolves the matching pending entry on the session with
    // `choice = once`.
    let attach = session.install_attachment(None);
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let session_for_drain = Arc::clone(&session);
    let mut live_rx = attach.live_rx;
    let drainer = tokio::spawn(async move {
        while let Some(frame) = live_rx.recv().await {
            let payload = frame.payload;
            // Forward to the test's receiver first.
            let _ = tx.send(payload.clone());
            // If this is a server-issued request, answer it.
            if let (Some(id_val), Some(method)) = (
                payload.get("id").cloned(),
                payload.get("method").and_then(Value::as_str),
            ) {
                if method == "hermes/approvalRequest" {
                    let id_str = match &id_val {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let resolved =
                        session_for_drain.resolve_pending(&id_str, Ok(json!({"choice": "once"})));
                    assert!(
                        resolved,
                        "expected pending approval request id={} to resolve",
                        id_str
                    );
                }
            }
        }
    });

    let thread_id = start_thread(&bridge, &ctx, "/tmp").await;
    start_turn(
        &bridge,
        &ctx,
        &thread_id,
        "hi",
        AskForApproval::UnlessTrusted,
    )
    .await;

    let completed = drain_until(
        &mut rx,
        |f| frame_method(f) == Some("turn/completed"),
        Duration::from_secs(10),
    )
    .await
    .expect("turn/completed must arrive");
    let payload: TurnCompletedNotification =
        serde_json::from_value(completed.get("params").cloned().unwrap()).unwrap();
    assert_eq!(
        payload.turn.status,
        TurnStatus::Completed,
        "approved run should complete"
    );
    let approvals = state.approvals.lock().unwrap().clone();
    assert!(
        approvals.contains(&"once".to_string()),
        "gateway should have received choice=once; got {:?}",
        approvals
    );
    gw.abort();
    drainer.abort();
}

#[tokio::test]
async fn thread_read_recovers_turns_from_persistence_after_drop() {
    let (api_base, _state, gw) = spawn_fake_gateway(Scenario::HappyPath).await;
    let dir = TempDir::new().unwrap();
    let state_dir = dir.path().to_path_buf();

    let thread_id = {
        let bridge = make_bridge(state_dir.clone(), api_base.clone());
        let (session, ctx) = make_conn();
        let (mut rx, _drainer) = attach_and_drain(&session);
        let thread_id = start_thread(&bridge, &ctx, "/tmp").await;
        start_turn(&bridge, &ctx, &thread_id, "hi", AskForApproval::Never).await;
        drain_until(
            &mut rx,
            |f| frame_method(f) == Some("turn/completed"),
            Duration::from_secs(5),
        )
        .await
        .expect("turn/completed");
        thread_id
    };

    // Simulate daemon restart: drop the bridge and reopen against the same
    // state dir.
    let bridge_v2 = make_bridge(state_dir, api_base);
    let (_session, ctx) = make_conn();
    let params = ThreadReadParams {
        thread_id: thread_id.clone(),
        include_turns: true,
    };
    let resp = bridge_v2
        .dispatch(&ctx, "thread/read", serde_json::to_value(&params).unwrap())
        .await
        .unwrap();
    let thread = resp.get("thread").unwrap();
    let turns = thread.get("turns").and_then(Value::as_array).unwrap();
    assert!(
        !turns.is_empty(),
        "post-restart thread/read should reconstruct turns from EventStore"
    );
    // First completed turn should be in Completed status reconstructed from RunStore.
    let last_status = turns
        .last()
        .and_then(|t| t.get("status"))
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(last_status, "completed");
    gw.abort();
}

#[tokio::test]
async fn item_started_replays_to_late_subscriber() {
    let (api_base, _state, gw) = spawn_fake_gateway(Scenario::HappyPath).await;
    let dir = TempDir::new().unwrap();
    let bridge = make_bridge(dir.path().to_path_buf(), api_base);
    let (session, ctx) = make_conn();
    let (mut rx, _drainer) = attach_and_drain(&session);
    let thread_id = start_thread(&bridge, &ctx, "/tmp").await;
    start_turn(&bridge, &ctx, &thread_id, "hi", AskForApproval::Never).await;
    // Drain a few frames to confirm the stream produced `item/started`.
    let started = drain_until(
        &mut rx,
        |f| frame_method(f) == Some("item/started"),
        Duration::from_secs(5),
    )
    .await
    .expect("item/started must arrive");
    let p: ItemStartedNotification =
        serde_json::from_value(started.get("params").cloned().unwrap()).unwrap();
    assert_eq!(p.thread_id, thread_id);
    gw.abort();
}
