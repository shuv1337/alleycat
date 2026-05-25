use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alleycat_codex_proto::jsonrpc::RequestId;
use alleycat_codex_proto::lifecycle::{
    ClientInfo, InitializeCapabilities, InitializeParams, InitializeResponse,
};
use alleycat_codex_proto::notifications::RemoteControlStatusChangedNotification;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::auth::read_auth_refresh_response;
use crate::jsonrpc::{
    InboundMessage, InboundNotification, InboundRequest, InboundResponse, Notification, Request,
    Response, redacted_json,
};
use crate::status::{CodexRemoteControlSnapshot, CodexRemoteControlState, now_unix_ms};
use crate::transport::JsonRpcTransport;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRemoteControlConfig {
    pub socket_path: PathBuf,
    pub auth_path: PathBuf,
    pub tuning: CodexRemoteControlTuning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRemoteControlTuning {
    pub interval: Duration,
    pub stale_after: Duration,
    pub connecting_grace: Duration,
    pub initial_status_wait: Duration,
    pub post_enable_wait: Duration,
    pub reconnect_delay: Duration,
    pub request_timeout: Duration,
}

impl Default for CodexRemoteControlTuning {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            stale_after: Duration::from_secs(300),
            connecting_grace: Duration::from_secs(120),
            initial_status_wait: Duration::from_secs(3),
            post_enable_wait: Duration::from_secs(3),
            reconnect_delay: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Default)]
pub struct CodexRemoteControlHandle {
    inner: Arc<HandleInner>,
}

#[derive(Default)]
struct HandleInner {
    state: RwLock<CodexRemoteControlSnapshot>,
    task: Mutex<Option<SupervisorTask>>,
}

struct SupervisorTask {
    config: CodexRemoteControlConfig,
    shutdown: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl CodexRemoteControlHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn status(&self) -> CodexRemoteControlSnapshot {
        self.inner.state.read().await.clone()
    }

    pub async fn start_or_update(&self, config: CodexRemoteControlConfig) {
        let mut task = self.inner.task.lock().await;
        let reuse = task
            .as_ref()
            .is_some_and(|task| task.config == config && !task.join.is_finished());
        if reuse {
            return;
        }

        if let Some(existing) = task.take() {
            stop_task(existing).await;
        }

        self.set_status(CodexRemoteControlSnapshot::connecting())
            .await;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let handle = self.clone();
        let task_config = config.clone();
        let join = tokio::spawn(async move {
            run_supervisor(handle, task_config, shutdown_rx).await;
        });
        *task = Some(SupervisorTask {
            config,
            shutdown,
            join,
        });
    }

    pub async fn stop(&self) {
        let task = self.inner.task.lock().await.take();
        if let Some(task) = task {
            stop_task(task).await;
        }
        self.set_status(CodexRemoteControlSnapshot::stopped()).await;
    }

    async fn set_status(&self, snapshot: CodexRemoteControlSnapshot) {
        *self.inner.state.write().await = snapshot;
    }

    async fn update_status(&self, update: impl FnOnce(&mut CodexRemoteControlSnapshot)) {
        let mut status = self.inner.state.write().await;
        update(&mut status);
    }
}

async fn stop_task(task: SupervisorTask) {
    let _ = task.shutdown.send(true);
    task.join.abort();
    let _ = task.join.await;
}

async fn run_supervisor(
    handle: CodexRemoteControlHandle,
    config: CodexRemoteControlConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        handle
            .set_status(CodexRemoteControlSnapshot::connecting())
            .await;
        match run_connection(&handle, &config, &mut shutdown).await {
            Ok(ConnectionExit::Shutdown) => return,
            Ok(ConnectionExit::Disconnected(message)) => {
                warn!(%message, "codex remote control disconnected");
                handle
                    .set_status(CodexRemoteControlSnapshot::error(message))
                    .await;
            }
            Err(error) => {
                let message = error.to_string();
                warn!(error = %message, "codex remote control loop failed");
                if handle.status().await.state != CodexRemoteControlState::Blocked {
                    handle
                        .set_status(CodexRemoteControlSnapshot::error(message))
                        .await;
                } else {
                    return;
                }
            }
        }
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return;
                }
            }
            _ = tokio::time::sleep(config.tuning.reconnect_delay) => {}
        }
    }
}

enum ConnectionExit {
    Shutdown,
    Disconnected(String),
}

async fn run_connection(
    handle: &CodexRemoteControlHandle,
    config: &CodexRemoteControlConfig,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<ConnectionExit> {
    let mut transport = JsonRpcTransport::connect(&config.socket_path).await?;
    let mut next_id = 1_i64;
    initialize(&mut transport, &mut next_id, config, handle, shutdown).await?;
    read_until_initial_status(&mut transport, handle, config, shutdown).await?;
    ensure_enabled(&mut transport, &mut next_id, handle, config, "initial").await?;
    read_for(
        &mut transport,
        handle,
        config,
        shutdown,
        config.tuning.post_enable_wait,
    )
    .await?;

    let mut interval = tokio::time::interval(config.tuning.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(ConnectionExit::Shutdown);
                }
            }
            _ = interval.tick() => {
                ensure_enabled(&mut transport, &mut next_id, handle, config, "interval").await?;
            }
            message = transport.read() => {
                match message {
                    Ok(message) => handle_inbound(&mut transport, handle, config, message).await?,
                    Err(error) => return Ok(ConnectionExit::Disconnected(error.to_string())),
                }
            }
        }
    }
}

async fn initialize(
    transport: &mut JsonRpcTransport,
    next_id: &mut i64,
    config: &CodexRemoteControlConfig,
    handle: &CodexRemoteControlHandle,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let request_id = take_id(next_id);
    let params = serde_json::to_value(InitializeParams {
        client_info: ClientInfo {
            // ChatGPT's remote-control environment registry classifies
            // environments by the app-server client identity that calls
            // `remoteControl/enable`. Unknown client names can connect
            // successfully but are omitted from the environments list shown
            // by ChatGPT/Codex Desktop. Match the official desktop backend
            // identity so the Alleycat-supervised Linux app-server remains
            // visible as a Codex Desktop remote target.
            name: "codex-backend".to_string(),
            title: Some("Codex Desktop".to_string()),
            version: "unknown".to_string(),
        },
        capabilities: Some(InitializeCapabilities {
            experimental_api: true,
            opt_out_notification_methods: None,
        }),
    })?;
    transport
        .send(&Request::new(request_id, "initialize", Some(params)))
        .await?;
    let result = wait_for_response(
        transport,
        handle,
        config,
        shutdown,
        RequestId::Integer(request_id),
        Duration::from_secs(10),
    )
    .await?;
    let response: InitializeResponse = serde_json::from_value(result)?;
    info!(
        user_agent = %response.user_agent,
        codex_home = %response.codex_home,
        "initialized codex remote-control app-server"
    );
    transport
        .send(&Notification::new("initialized", Some(json!({}))))
        .await?;
    Ok(())
}

async fn wait_for_response(
    transport: &mut JsonRpcTransport,
    handle: &CodexRemoteControlHandle,
    config: &CodexRemoteControlConfig,
    shutdown: &mut watch::Receiver<bool>,
    request_id: RequestId,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    anyhow::bail!("shutdown while waiting for response id={request_id}");
                }
            }
            _ = &mut deadline => {
                anyhow::bail!("timed out waiting for response id={request_id}");
            }
            message = transport.read() => {
                match message? {
                    InboundMessage::Response(response) if response.id == request_id => {
                        return response_result(response);
                    }
                    message => handle_inbound(transport, handle, config, message).await?,
                }
            }
        }
    }
}

fn response_result(response: InboundResponse) -> anyhow::Result<Value> {
    if let Some(error) = response.error {
        anyhow::bail!("json-rpc error {}: {}", error.code, error.message);
    }
    Ok(response.result.unwrap_or(Value::Object(Default::default())))
}

async fn read_until_initial_status(
    transport: &mut JsonRpcTransport,
    handle: &CodexRemoteControlHandle,
    config: &CodexRemoteControlConfig,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    read_for(
        transport,
        handle,
        config,
        shutdown,
        config.tuning.initial_status_wait,
    )
    .await
}

async fn read_for(
    transport: &mut JsonRpcTransport,
    handle: &CodexRemoteControlHandle,
    config: &CodexRemoteControlConfig,
    shutdown: &mut watch::Receiver<bool>,
    duration: Duration,
) -> anyhow::Result<()> {
    if duration.is_zero() {
        return Ok(());
    }
    let deadline = tokio::time::sleep(duration);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(());
                }
            }
            _ = &mut deadline => return Ok(()),
            message = transport.read() => {
                handle_inbound(transport, handle, config, message?).await?;
            }
        }
    }
}

async fn ensure_enabled(
    transport: &mut JsonRpcTransport,
    next_id: &mut i64,
    handle: &CodexRemoteControlHandle,
    config: &CodexRemoteControlConfig,
    source: &str,
) -> anyhow::Result<()> {
    let snapshot = handle.status().await;
    let stale = is_stale(&snapshot, config.tuning.stale_after);
    if is_healthy(&snapshot, config.tuning.connecting_grace) && !stale {
        debug!(source, state = ?snapshot.state, "codex remote control healthy");
        return Ok(());
    }

    let reason = unhealthy_reason(&snapshot, stale);
    info!(source, reason = %reason, "calling remoteControl/enable");
    let request_id = take_id(next_id);
    transport
        .send(&Request::new(request_id, "remoteControl/enable", None))
        .await?;
    handle
        .update_status(|snapshot| {
            snapshot.last_enable_reason = Some(reason.clone());
        })
        .await;

    let result = wait_for_response_with_background(
        transport,
        handle,
        config,
        RequestId::Integer(request_id),
        config.tuning.request_timeout,
    )
    .await?;
    apply_enable_result(handle, result, reason).await;
    Ok(())
}

async fn wait_for_response_with_background(
    transport: &mut JsonRpcTransport,
    handle: &CodexRemoteControlHandle,
    config: &CodexRemoteControlConfig,
    request_id: RequestId,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                anyhow::bail!("timed out waiting for response id={request_id}");
            }
            message = transport.read() => {
                match message? {
                    InboundMessage::Response(response) if response.id == request_id => {
                        return response_result(response);
                    }
                    message => handle_inbound(transport, handle, config, message).await?,
                }
            }
        }
    }
}

async fn apply_enable_result(handle: &CodexRemoteControlHandle, result: Value, reason: String) {
    match serde_json::from_value::<RemoteControlStatusChangedNotification>(result) {
        Ok(status) => {
            handle
                .update_status(|snapshot| {
                    snapshot.apply_status_changed(status);
                    snapshot.last_enable_reason = Some(reason);
                })
                .await;
        }
        Err(_) => {
            handle
                .update_status(|snapshot| {
                    snapshot.last_enable_reason = Some(reason);
                })
                .await;
        }
    }
}

async fn handle_inbound(
    transport: &mut JsonRpcTransport,
    handle: &CodexRemoteControlHandle,
    config: &CodexRemoteControlConfig,
    message: InboundMessage,
) -> anyhow::Result<()> {
    match message {
        InboundMessage::Notification(notification) => {
            handle_notification(handle, notification).await?;
        }
        InboundMessage::Request(request) => {
            handle_request(transport, handle, config, request).await?;
        }
        InboundMessage::Response(response) => {
            debug!(id = %response.id, "ignoring unrelated json-rpc response");
        }
    }
    Ok(())
}

async fn handle_notification(
    handle: &CodexRemoteControlHandle,
    notification: InboundNotification,
) -> anyhow::Result<()> {
    if notification.method != "remoteControl/status/changed" {
        debug!(method = %notification.method, "ignoring codex notification");
        return Ok(());
    }
    let params = notification
        .params
        .unwrap_or(Value::Object(Default::default()));
    let status: RemoteControlStatusChangedNotification = serde_json::from_value(params)?;
    handle
        .update_status(|snapshot| {
            snapshot.apply_status_changed(status);
        })
        .await;
    Ok(())
}

async fn handle_request(
    transport: &mut JsonRpcTransport,
    handle: &CodexRemoteControlHandle,
    config: &CodexRemoteControlConfig,
    request: InboundRequest,
) -> anyhow::Result<()> {
    match request.method.as_str() {
        "account/chatgptAuthTokens/refresh" => {
            let result = read_auth_refresh_response(&config.auth_path).await?;
            transport
                .send(&Response::result(request.id, serde_json::to_value(result)?))
                .await?;
        }
        "attestation/generate" => {
            let params = request.params.unwrap_or(Value::Object(Default::default()));
            let message = format!(
                "attestation/generate requested params={}",
                redacted_json(&params)
            );
            transport
                .send(&Response::error(
                    request.id,
                    -32000,
                    "alleycat native codex remote control does not provide attestation",
                ))
                .await?;
            handle
                .set_status(CodexRemoteControlSnapshot::blocked(message.clone()))
                .await;
            anyhow::bail!(message);
        }
        other => {
            let params = request
                .params
                .unwrap_or_else(|| Value::Object(Default::default()));
            warn!(
                method = %other,
                params = %redacted_json(&params),
                "unsupported codex server request"
            );
            transport
                .send(&Response::error(
                    request.id,
                    -32601,
                    format!("unsupported request {other}"),
                ))
                .await?;
        }
    }
    Ok(())
}

fn take_id(next_id: &mut i64) -> i64 {
    let id = *next_id;
    *next_id += 1;
    id
}

fn is_healthy(snapshot: &CodexRemoteControlSnapshot, connecting_grace: Duration) -> bool {
    match snapshot.state {
        CodexRemoteControlState::Connected => true,
        CodexRemoteControlState::Connecting => {
            snapshot.last_update_unix_ms.is_some_and(|updated| {
                now_unix_ms().saturating_sub(updated) < millis(connecting_grace)
            })
        }
        _ => false,
    }
}

fn is_stale(snapshot: &CodexRemoteControlSnapshot, stale_after: Duration) -> bool {
    snapshot
        .last_update_unix_ms
        .is_none_or(|updated| now_unix_ms().saturating_sub(updated) >= millis(stale_after))
}

fn unhealthy_reason(snapshot: &CodexRemoteControlSnapshot, stale: bool) -> String {
    if snapshot.last_update_unix_ms.is_none() {
        return "missing-status".to_string();
    }
    if stale {
        return format!("stale-status:{}", state_reason(snapshot.state));
    }
    format!("status:{}", state_reason(snapshot.state))
}

fn state_reason(state: CodexRemoteControlState) -> &'static str {
    match state {
        CodexRemoteControlState::Idle => "idle",
        CodexRemoteControlState::Connecting => "connecting",
        CodexRemoteControlState::Disabled => "disabled",
        CodexRemoteControlState::Connected => "connected",
        CodexRemoteControlState::Errored => "errored",
        CodexRemoteControlState::Blocked => "blocked",
        CodexRemoteControlState::Stopped => "stopped",
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::{SinkExt, StreamExt};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::net::UnixListener;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    #[test]
    fn health_decision_requests_enable_for_missing_disabled_errored_and_stale() {
        let tuning = CodexRemoteControlTuning {
            stale_after: Duration::from_millis(10),
            connecting_grace: Duration::from_secs(10),
            ..CodexRemoteControlTuning::default()
        };
        let missing = CodexRemoteControlSnapshot::default();
        assert!(!is_healthy(&missing, tuning.connecting_grace));
        assert!(is_stale(&missing, tuning.stale_after));
        assert_eq!(unhealthy_reason(&missing, true), "missing-status");

        let disabled = snapshot(CodexRemoteControlState::Disabled, 0);
        assert!(!is_healthy(&disabled, tuning.connecting_grace));
        assert_eq!(unhealthy_reason(&disabled, false), "status:disabled");

        let errored = snapshot(CodexRemoteControlState::Errored, 0);
        assert!(!is_healthy(&errored, tuning.connecting_grace));
        assert_eq!(unhealthy_reason(&errored, false), "status:errored");

        let connected = snapshot(CodexRemoteControlState::Connected, 0);
        assert!(is_healthy(&connected, tuning.connecting_grace));
        assert!(!is_stale(&connected, tuning.stale_after));

        let stale = snapshot(CodexRemoteControlState::Connected, 50);
        assert!(is_healthy(&stale, tuning.connecting_grace));
        assert!(is_stale(&stale, tuning.stale_after));
        assert_eq!(unhealthy_reason(&stale, true), "stale-status:connected");
    }

    #[tokio::test]
    async fn fake_server_observes_initialize_enable_and_auth_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("codex.sock");
        let auth_path = temp.path().join("auth.json");
        tokio::fs::write(
            &auth_path,
            r#"{"tokens":{"access_token":"secret-token","account_id":"acct_123"}}"#,
        )
        .await
        .unwrap();
        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_observed = Arc::clone(&observed);
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            let init = read_json(&mut ws).await;
            server_observed
                .lock()
                .await
                .push(init["method"].as_str().unwrap().to_string());
            assert_eq!(init["params"]["clientInfo"]["name"], "codex-backend");
            assert_eq!(init["params"]["clientInfo"]["title"], "Codex Desktop");
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": init["id"].clone(),
                    "result": {
                        "userAgent": "codex-test",
                        "codexHome": "/tmp/codex",
                        "platformFamily": "unix",
                        "platformOs": "linux"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            let initialized = read_json(&mut ws).await;
            server_observed
                .lock()
                .await
                .push(initialized["method"].as_str().unwrap().to_string());
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "method": "remoteControl/status/changed",
                    "params": {
                        "status": "disabled",
                        "serverName": "test-server",
                        "environmentId": "env-before"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            let enable = read_json(&mut ws).await;
            server_observed
                .lock()
                .await
                .push(enable["method"].as_str().unwrap().to_string());
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": enable["id"].clone(),
                    "result": {
                        "status": "connected",
                        "serverName": "test-server",
                        "environmentId": "env-after"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 99,
                    "method": "account/chatgptAuthTokens/refresh",
                    "params": {"reason": "test"}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            let auth = read_json(&mut ws).await;
            assert_eq!(auth["id"], 99);
            assert_eq!(auth["result"]["accessToken"], "secret-token");
            assert_eq!(auth["result"]["chatgptAccountId"], "acct_123");
        });

        let handle = CodexRemoteControlHandle::new();
        handle
            .start_or_update(CodexRemoteControlConfig {
                socket_path,
                auth_path,
                tuning: CodexRemoteControlTuning {
                    initial_status_wait: Duration::from_millis(100),
                    post_enable_wait: Duration::from_millis(300),
                    interval: Duration::from_secs(60),
                    reconnect_delay: Duration::from_secs(60),
                    request_timeout: Duration::from_secs(5),
                    ..CodexRemoteControlTuning::default()
                },
            })
            .await;

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        handle.stop().await;
        let methods = observed.lock().await.clone();
        assert_eq!(
            methods,
            vec!["initialize", "initialized", "remoteControl/enable"]
        );
        let status = handle.status().await;
        assert_eq!(status.state, CodexRemoteControlState::Stopped);
    }

    #[tokio::test]
    async fn attestation_request_records_blocked_without_secret_leak() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("codex.sock");
        let auth_path = temp.path().join("auth.json");
        tokio::fs::write(
            &auth_path,
            r#"{"tokens":{"access_token":"secret-token","account_id":"acct_123"}}"#,
        )
        .await
        .unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let init = read_json(&mut ws).await;
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": init["id"].clone(),
                    "result": {
                        "userAgent": "codex-test",
                        "codexHome": "/tmp/codex",
                        "platformFamily": "unix",
                        "platformOs": "linux"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            let _initialized = read_json(&mut ws).await;
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 77,
                    "method": "attestation/generate",
                    "params": {"accessToken": "secret-token", "nonce": "abc"}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            let response = read_json(&mut ws).await;
            assert_eq!(response["id"], 77);
            assert_eq!(response["error"]["code"], -32000);
        });

        let handle = CodexRemoteControlHandle::new();
        handle
            .start_or_update(CodexRemoteControlConfig {
                socket_path,
                auth_path,
                tuning: CodexRemoteControlTuning {
                    initial_status_wait: Duration::from_millis(300),
                    reconnect_delay: Duration::from_secs(60),
                    request_timeout: Duration::from_secs(5),
                    ..CodexRemoteControlTuning::default()
                },
            })
            .await;

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        let status = wait_for_state(&handle, CodexRemoteControlState::Blocked).await;
        let blocked = status.blocked.unwrap_or_default();
        assert!(blocked.contains("attestation/generate"));
        assert!(blocked.contains("<redacted>"));
        assert!(!blocked.contains("secret-token"));
        handle.stop().await;
    }

    fn snapshot(state: CodexRemoteControlState, age_ms: u64) -> CodexRemoteControlSnapshot {
        CodexRemoteControlSnapshot {
            state,
            last_update_unix_ms: Some(now_unix_ms().saturating_sub(age_ms)),
            ..CodexRemoteControlSnapshot::default()
        }
    }

    async fn wait_for_state(
        handle: &CodexRemoteControlHandle,
        state: CodexRemoteControlState,
    ) -> CodexRemoteControlSnapshot {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = handle.status().await;
            if status.state == state {
                return status;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {state:?}, last status={status:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn read_json(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
    ) -> Value {
        loop {
            let message = ws.next().await.unwrap().unwrap();
            match message {
                Message::Text(text) => return serde_json::from_str(text.as_ref()).unwrap(),
                Message::Binary(bytes) => return serde_json::from_slice(bytes.as_ref()).unwrap(),
                Message::Ping(bytes) => ws.send(Message::Pong(bytes)).await.unwrap(),
                Message::Pong(_) => {}
                Message::Close(_) => panic!("websocket closed"),
                Message::Frame(_) => panic!("unexpected raw frame"),
            }
        }
    }
}
