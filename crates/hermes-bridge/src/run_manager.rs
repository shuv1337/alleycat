//! Single-owner SSE pump + broadcast for active Hermes runs.
//!
//! Phase 0 confirmed `GET /v1/runs/{id}/events` is single-consumer and the
//! gateway pops the per-run queue on consumer disconnect. To support both
//! reconnect-within-Alleycat-session and multi-client fan-out, the manager
//! owns the **only** SSE consumer for a run's lifetime and broadcasts
//! normalized events to internal subscribers (the per-`Conn` translation
//! tasks). All events are persisted to [`EventStore`] before broadcast so
//! reconnects can replay missed frames.
//!
//! If the SSE pump dies mid-run (network glitch, gateway crash) before a
//! terminal event, the manager polls `GET /v1/runs/{run_id}` as the
//! gateway-side fallback. That endpoint persists terminal status for
//! ~3600 seconds.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alleycat_codex_proto::common::TurnError;
use alleycat_codex_proto::common::TurnStatus;
use alleycat_codex_proto::items::ThreadItem;
use alleycat_codex_proto::notifications::{
    AgentMessageDeltaNotification, ItemCompletedNotification, ItemStartedNotification,
    TurnCompletedNotification,
};
use alleycat_codex_proto::thread::Turn;
use anyhow::Result;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::api_client::HermesApiClient;
use crate::event_store::{EventStore, NormalizedHermesEvent};
use crate::run_state::{HermesRunRecord, HermesTurnStatus, RunStore};

/// Channel buffer for in-flight events handed to subscribers. Hermes deltas
/// are small; 256 covers the worst-case while still bounding memory.
const BROADCAST_CAPACITY: usize = 256;

/// Polling interval used to recover terminal status after pump death.
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Maximum total wait when polling `GET /v1/runs/{id}` after pump death.
const STATUS_POLL_MAX: Duration = Duration::from_secs(30);

fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Final outcome of a run from the manager's perspective. The terminal frame
/// (`turn/completed` notification with the correct status) is already
/// persisted to the event store before this is observed by subscribers via
/// the broadcast.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum RunOutcome {
    Completed { accumulated_text: String },
    Failed { error: String },
    Cancelled,
}

#[allow(dead_code)]
struct ActiveRun {
    run_id: String,
    turn_id: String,
    tx: broadcast::Sender<NormalizedHermesEvent>,
    /// Pump task handle, retained for graceful join on shutdown if needed.
    task: JoinHandle<()>,
}

/// Snapshot of an in-flight subscription handed to a connection-level
/// translator.
pub struct RunSubscription {
    pub rx: broadcast::Receiver<NormalizedHermesEvent>,
    /// Already-persisted events for replay. The translator should emit these
    /// before consuming `rx`. May be empty for a brand-new subscription.
    pub replay: Vec<NormalizedHermesEvent>,
}

/// Manages active Hermes runs across the bridge.
pub struct HermesRunManager {
    client: Arc<HermesApiClient>,
    run_store: Arc<RunStore>,
    event_store: Arc<EventStore>,
    active: Mutex<HashMap<String, Arc<ActiveRun>>>,
}

impl HermesRunManager {
    pub fn new(
        client: Arc<HermesApiClient>,
        run_store: Arc<RunStore>,
        event_store: Arc<EventStore>,
    ) -> Self {
        Self {
            client,
            run_store,
            event_store,
            active: Mutex::new(HashMap::new()),
        }
    }

    /// Spawn (or attach to) the single SSE pump for `run_id`. Idempotent —
    /// repeated calls with the same `run_id` return the existing handle. The
    /// caller is responsible for having marked the run `Running` in
    /// [`RunStore`] before calling this.
    ///
    /// `agent_item_id` is the stable id used for the `item/started`/`item/
    /// completed` correlation of the assistant message.
    /// `auto_approve_once` controls fallback behavior when the gateway emits
    /// `approval.request` and no client-side approval handler responds in
    /// time. When true, the manager posts `choice=once` to keep the run
    /// going (for `approval_policy=Never` semantics).
    pub fn ensure_run(
        self: &Arc<Self>,
        record: HermesRunRecord,
        agent_item_id: String,
        auto_approve_once: bool,
    ) -> Arc<broadcast::Sender<NormalizedHermesEvent>> {
        let run_id = record
            .run_id
            .clone()
            .expect("ensure_run requires record.run_id");
        let turn_id = record.turn_id.clone();
        let mut active = self.active.lock().unwrap();
        if let Some(existing) = active.get(&run_id) {
            return Arc::new(existing.tx.clone());
        }
        let (tx, _rx0) = broadcast::channel::<NormalizedHermesEvent>(BROADCAST_CAPACITY);
        let tx_arc = Arc::new(tx.clone());
        let manager = Arc::clone(self);
        let pump_run_id = run_id.clone();
        let pump_turn_id = turn_id.clone();
        let pump_item_id = agent_item_id.clone();
        let pump_record = record.clone();
        let task = tokio::spawn(async move {
            manager
                .pump(
                    pump_record,
                    pump_run_id,
                    pump_turn_id,
                    pump_item_id,
                    auto_approve_once,
                )
                .await;
        });
        active.insert(
            run_id.clone(),
            Arc::new(ActiveRun {
                run_id,
                turn_id,
                tx,
                task,
            }),
        );
        tx_arc
    }

    /// Subscribe to events for an active run, with replay of any already-
    /// persisted events for that run. If the run is not active in memory,
    /// `rx` will be closed and only `replay` is populated.
    pub fn subscribe(&self, run_id: &str, after_seq: u64) -> RunSubscription {
        let replay = self
            .event_store
            .read_since(run_id, after_seq)
            .unwrap_or_else(|err| {
                warn!(run_id = %run_id, error = %err, "hermes event_store: replay failed");
                Vec::new()
            });
        let active = self.active.lock().unwrap();
        if let Some(existing) = active.get(run_id) {
            RunSubscription {
                rx: existing.tx.subscribe(),
                replay,
            }
        } else {
            // Construct a dummy receiver that immediately returns Closed.
            let (tx, rx) = broadcast::channel::<NormalizedHermesEvent>(1);
            drop(tx);
            RunSubscription { rx, replay }
        }
    }

    /// Best-effort stop. Idempotent.
    pub async fn stop(&self, run_id: &str) -> Result<()> {
        self.client.stop_run(run_id).await
    }

    /// Test/internal accessor: are we tracking this run?
    #[allow(dead_code)]
    pub fn is_active(&self, run_id: &str) -> bool {
        self.active.lock().unwrap().contains_key(run_id)
    }

    /// Number of currently-tracked active runs.
    #[allow(dead_code)]
    pub fn active_count(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    /// Forward an approval decision to the gateway. The choice maps directly
    /// to the gateway's accepted values.
    pub async fn submit_approval(
        &self,
        run_id: &str,
        choice: crate::api_client::ApprovalChoice,
        resolve_all: bool,
    ) -> Result<()> {
        self.client
            .resolve_run_approval(run_id, choice, resolve_all)
            .await
    }

    // ---- internal pump ----

    async fn pump(
        self: Arc<Self>,
        record: HermesRunRecord,
        run_id: String,
        turn_id: String,
        agent_item_id: String,
        auto_approve_once: bool,
    ) {
        let thread_id = record.thread_id.clone();
        info!(
            run_id = %run_id,
            turn_id = %turn_id,
            thread_id = %thread_id,
            agent = "hermes",
            "hermes run pump start"
        );

        // Emit the `item/started` notification for the assistant message
        // so reconnecting clients can replay it.
        self.emit_agent_started(&run_id, &turn_id, &thread_id, &agent_item_id);

        let mut full_text = String::new();
        let outcome = self
            .pump_events(
                &run_id,
                &turn_id,
                &thread_id,
                &agent_item_id,
                &mut full_text,
                auto_approve_once,
            )
            .await;

        // Persist accumulated text.
        let _ = self.run_store.touch_text(&turn_id, &full_text, epoch_ms());

        // Emit terminal notifications regardless of outcome.
        let (status, error_text) = match &outcome {
            RunOutcome::Completed { .. } => (HermesTurnStatus::Completed, None),
            RunOutcome::Failed { error } => (HermesTurnStatus::Failed, Some(error.clone())),
            RunOutcome::Cancelled => (HermesTurnStatus::Cancelled, None),
        };
        self.emit_terminal(
            &run_id,
            &turn_id,
            &thread_id,
            &agent_item_id,
            &full_text,
            status,
            error_text.clone(),
        );

        // Mark run record terminal.
        let now = epoch_ms();
        let _ = self.run_store.mark_terminal(
            &turn_id,
            status,
            error_text,
            Some(full_text.clone()),
            now,
        );

        // Remove from active set.
        self.active.lock().unwrap().remove(&run_id);
        info!(
            run_id = %run_id,
            turn_id = %turn_id,
            agent = "hermes",
            outcome = ?outcome,
            "hermes run pump end"
        );
    }

    async fn pump_events(
        self: &Arc<Self>,
        run_id: &str,
        turn_id: &str,
        thread_id: &str,
        agent_item_id: &str,
        full_text: &mut String,
        auto_approve_once: bool,
    ) -> RunOutcome {
        let resp = match self.client.events_stream(run_id).await {
            Ok(resp) => resp,
            Err(err) => {
                let message = format!("Hermes events error: {err}");
                warn!(run_id = %run_id, error = %err, "hermes events_stream open failed");
                return self
                    .fallback_to_status_poll(run_id, &message, full_text)
                    .await;
            }
        };
        let mut body = String::new();
        let mut stream = resp.bytes_stream();
        let mut had_terminal_event = false;
        let mut terminal_text: Option<String> = None;
        let mut terminal_error: Option<String> = None;
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(err) => {
                    let message = format!("Hermes SSE error: {err}");
                    warn!(run_id = %run_id, error = %err, "hermes events stream chunk error");
                    return self
                        .fallback_to_status_poll(run_id, &message, full_text)
                        .await;
                }
            };
            body.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(idx) = body.find("\n\n") {
                let complete = body[..idx + 2].to_string();
                body = body[idx + 2..].to_string();
                for event in crate::sse::parse_sse_frames(&complete) {
                    match self
                        .translate_event(
                            run_id,
                            turn_id,
                            thread_id,
                            agent_item_id,
                            full_text,
                            auto_approve_once,
                            event,
                        )
                        .await
                    {
                        TranslationOutcome::Continue => {}
                        TranslationOutcome::TerminalSuccess { text } => {
                            had_terminal_event = true;
                            terminal_text = Some(text);
                            break;
                        }
                        TranslationOutcome::TerminalError { message } => {
                            had_terminal_event = true;
                            terminal_error = Some(message);
                            break;
                        }
                    }
                }
                if had_terminal_event {
                    break;
                }
            }
            if had_terminal_event {
                break;
            }
        }
        // Drain any trailing un-`\n\n`-terminated frame (regression-tested:
        // could contain a terminal event).
        if !had_terminal_event && !body.trim().is_empty() {
            for event in crate::sse::parse_sse_frames(&body) {
                match self
                    .translate_event(
                        run_id,
                        turn_id,
                        thread_id,
                        agent_item_id,
                        full_text,
                        auto_approve_once,
                        event,
                    )
                    .await
                {
                    TranslationOutcome::Continue => {}
                    TranslationOutcome::TerminalSuccess { text } => {
                        had_terminal_event = true;
                        terminal_text = Some(text);
                        break;
                    }
                    TranslationOutcome::TerminalError { message } => {
                        had_terminal_event = true;
                        terminal_error = Some(message);
                        break;
                    }
                }
            }
        }
        if had_terminal_event {
            if let Some(err) = terminal_error {
                return RunOutcome::Failed { error: err };
            }
            // Use any explicit terminal `output` (when the run produced text
            // outside the delta stream).
            if let Some(text) = terminal_text {
                if full_text.is_empty() && !text.is_empty() {
                    *full_text = text;
                }
            }
            return RunOutcome::Completed {
                accumulated_text: full_text.clone(),
            };
        }
        // Stream ended without a terminal frame — fall back to status poll.
        let message = "Hermes SSE stream ended without terminal event".to_string();
        warn!(run_id = %run_id, "hermes events stream ended without terminal");
        self.fallback_to_status_poll(run_id, &message, full_text)
            .await
    }

    async fn translate_event(
        self: &Arc<Self>,
        run_id: &str,
        turn_id: &str,
        thread_id: &str,
        agent_item_id: &str,
        full_text: &mut String,
        auto_approve_once: bool,
        event: crate::api_client::HermesEvent,
    ) -> TranslationOutcome {
        if let Some(delta) = event.message_delta() {
            full_text.push_str(&delta);
            self.emit_agent_delta(run_id, turn_id, thread_id, agent_item_id, &delta);
            return TranslationOutcome::Continue;
        }
        if let Some(err) = event.terminal_error() {
            let message = format!("Hermes API error: {err}");
            return TranslationOutcome::TerminalError { message };
        }
        if event.event == "approval.request" {
            if auto_approve_once {
                if let Err(err) = self
                    .client
                    .resolve_run_approval(run_id, crate::api_client::ApprovalChoice::Once, false)
                    .await
                {
                    return TranslationOutcome::TerminalError {
                        message: format!("Hermes approval error: {err}"),
                    };
                }
                self.persist_and_publish(
                    run_id,
                    turn_id,
                    thread_id,
                    "hermes/approvalResponded",
                    json!({
                        "thread_id": thread_id,
                        "turn_id": turn_id,
                        "run_id": run_id,
                        "data": {
                            "choice": "once",
                            "autoApproved": true
                        },
                    }),
                );
                return TranslationOutcome::Continue;
            }
            // Publish a normalized approval event so a connection-level
            // translator can prompt the client through `notifier().request`.
            // The manager is back-end-agnostic: it only knows the gateway
            // emitted an approval request. The Phase 6 connection handler
            // decides how to surface that to the user.
            self.persist_and_publish(
                run_id,
                turn_id,
                thread_id,
                "hermes/approvalRequest",
                json!({
                    "thread_id": thread_id,
                    "turn_id": turn_id,
                    "run_id": run_id,
                    "data": event.data,
                }),
            );
            return TranslationOutcome::Continue;
        }
        if event.event == "approval.responded" {
            // Forward as informational notification for now; clients may
            // ignore.
            self.persist_and_publish(
                run_id,
                turn_id,
                thread_id,
                "hermes/approvalResponded",
                json!({
                    "thread_id": thread_id,
                    "turn_id": turn_id,
                    "run_id": run_id,
                    "data": event.data,
                }),
            );
            return TranslationOutcome::Continue;
        }
        if event.is_terminal_success() {
            let mut text = String::new();
            if let Some(output) = event.data.get("output").and_then(Value::as_str) {
                text = output.to_string();
                if full_text.is_empty() && !text.is_empty() {
                    self.emit_agent_delta(run_id, turn_id, thread_id, agent_item_id, &text);
                    full_text.push_str(&text);
                }
            }
            return TranslationOutcome::TerminalSuccess { text };
        }
        // Other events (tool.started/completed, reasoning.available, etc.)
        // are not yet surfaced to clients. They still get persisted so
        // future replay/inspection can use them.
        self.persist_and_publish(
            run_id,
            turn_id,
            thread_id,
            "hermes/rawEvent",
            json!({
                "thread_id": thread_id,
                "turn_id": turn_id,
                "run_id": run_id,
                "event": event.event,
                "data": event.data,
            }),
        );
        TranslationOutcome::Continue
    }

    /// Poll `GET /v1/runs/{run_id}` for terminal status after SSE pump death.
    async fn fallback_to_status_poll(
        self: &Arc<Self>,
        run_id: &str,
        sse_error: &str,
        full_text: &mut String,
    ) -> RunOutcome {
        let deadline = tokio::time::Instant::now() + STATUS_POLL_MAX;
        loop {
            match self.client.get_run_status(run_id).await {
                Ok(Some(status)) => {
                    if status.is_terminal() {
                        // If the gateway has a final `output`, treat that as
                        // the accumulated text when we missed deltas.
                        if let Some(output) = status.output.clone() {
                            if full_text.is_empty() && !output.is_empty() {
                                *full_text = output;
                            }
                        }
                        return match status.status.as_str() {
                            "completed" => RunOutcome::Completed {
                                accumulated_text: full_text.clone(),
                            },
                            "cancelled" => RunOutcome::Cancelled,
                            _ => RunOutcome::Failed {
                                error: status.error.unwrap_or_else(|| {
                                    format!("Hermes run terminal status={}", status.status)
                                }),
                            },
                        };
                    }
                }
                Ok(None) => {
                    debug!(run_id = %run_id, "status poll: 404 run_not_found");
                    // The gateway can lose the run record (e.g. after the
                    // _RUN_STATUS_TTL window). Treat as failed with the
                    // original SSE error.
                    return RunOutcome::Failed {
                        error: format!("{sse_error}; gateway lost run record"),
                    };
                }
                Err(err) => {
                    warn!(run_id = %run_id, error = %err, "status poll error");
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return RunOutcome::Failed {
                    error: format!(
                        "{sse_error}; status poll did not observe terminal state within {:?}",
                        STATUS_POLL_MAX
                    ),
                };
            }
            sleep(STATUS_POLL_INTERVAL).await;
        }
    }

    fn emit_agent_started(
        &self,
        run_id: &str,
        turn_id: &str,
        thread_id: &str,
        agent_item_id: &str,
    ) {
        let item = ThreadItem::AgentMessage {
            id: agent_item_id.to_string(),
            text: String::new(),
            phase: None,
            memory_citation: None,
        };
        let payload = ItemStartedNotification {
            item,
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            parent_item_id: None,
        };
        let value = match serde_json::to_value(&payload) {
            Ok(v) => v,
            Err(err) => {
                warn!(error = %err, "serialize item/started");
                return;
            }
        };
        self.persist_and_publish(run_id, turn_id, thread_id, "item/started", value);
    }

    fn emit_agent_delta(
        &self,
        run_id: &str,
        turn_id: &str,
        thread_id: &str,
        agent_item_id: &str,
        delta: &str,
    ) {
        let payload = AgentMessageDeltaNotification {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            item_id: agent_item_id.to_string(),
            delta: delta.to_string(),
            parent_item_id: None,
        };
        let value = match serde_json::to_value(&payload) {
            Ok(v) => v,
            Err(err) => {
                warn!(error = %err, "serialize agentMessage delta");
                return;
            }
        };
        self.persist_and_publish(run_id, turn_id, thread_id, "item/agentMessage/delta", value);
    }

    fn emit_terminal(
        &self,
        run_id: &str,
        turn_id: &str,
        thread_id: &str,
        agent_item_id: &str,
        full_text: &str,
        status: HermesTurnStatus,
        error: Option<String>,
    ) {
        // item/completed for the assistant message.
        let agent_item = ThreadItem::AgentMessage {
            id: agent_item_id.to_string(),
            text: full_text.to_string(),
            phase: None,
            memory_citation: None,
        };
        let item_completed = ItemCompletedNotification {
            item: agent_item,
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            parent_item_id: None,
        };
        if let Ok(value) = serde_json::to_value(&item_completed) {
            self.persist_and_publish(run_id, turn_id, thread_id, "item/completed", value);
        }
        let turn_status = match status {
            HermesTurnStatus::Completed => TurnStatus::Completed,
            HermesTurnStatus::Failed => TurnStatus::Failed,
            HermesTurnStatus::Cancelled => TurnStatus::Failed,
            _ => TurnStatus::Completed,
        };
        let now = epoch_ms();
        let turn = Turn {
            id: turn_id.to_string(),
            items: vec![],
            items_view: "full".to_string(),
            status: turn_status,
            error: error.map(|message| TurnError {
                message,
                codex_error_info: None,
                additional_details: None,
            }),
            started_at: Some(now),
            completed_at: Some(now),
            duration_ms: None,
        };
        let turn_completed = TurnCompletedNotification {
            thread_id: thread_id.to_string(),
            turn,
        };
        if let Ok(value) = serde_json::to_value(&turn_completed) {
            self.persist_and_publish(run_id, turn_id, thread_id, "turn/completed", value);
        }
    }

    fn persist_and_publish(
        &self,
        run_id: &str,
        turn_id: &str,
        thread_id: &str,
        method: &str,
        params: Value,
    ) -> Option<NormalizedHermesEvent> {
        let event = NormalizedHermesEvent {
            seq: 0,
            ts: epoch_ms(),
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            run_id: run_id.to_string(),
            method: method.to_string(),
            params,
        };
        let persisted = match self.event_store.append(run_id, event) {
            Ok(ev) => ev,
            Err(err) => {
                warn!(run_id = %run_id, error = %err, method = %method, "hermes event_store append failed");
                return None;
            }
        };
        let _ = self
            .run_store
            .note_event_seq(turn_id, persisted.seq, epoch_ms());
        // Broadcast best-effort: if there are no subscribers, that's fine —
        // the persistence above is the durable record.
        let active = self.active.lock().unwrap();
        if let Some(handle) = active.get(run_id) {
            let _ = handle.tx.send(persisted.clone());
        }
        Some(persisted)
    }
}

enum TranslationOutcome {
    Continue,
    TerminalSuccess { text: String },
    TerminalError { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn manager_can_be_constructed() {
        let client = Arc::new(HermesApiClient::new("http://127.0.0.1:1", None));
        let run_store = Arc::new(RunStore::new_in_memory());
        let event_store = Arc::new(EventStore::new_in_memory());
        let mgr = HermesRunManager::new(client, run_store, event_store);
        assert_eq!(mgr.active_count(), 0);
        assert!(!mgr.is_active("run-x"));
    }
}
