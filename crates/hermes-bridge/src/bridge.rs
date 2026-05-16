//! Hermes Bridge — Bridge trait implementation.
//!
//! Translates between the codex app-server JSON-RPC surface and the
//! Hermes Agent backend (API or CLI mode). Core chat/thread methods are
//! implemented; unsupported Codex-only features return explicit -32601 errors.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::broadcast::error::RecvError;

use alleycat_bridge_core::{Bridge, Conn, JsonRpcError};
use alleycat_codex_proto::account::{
    Account, CancelLoginAccountResponse, CancelLoginAccountStatus, GetAccountRateLimitsResponse,
    GetAccountResponse, LoginAccountResponse, LogoutAccountResponse,
};
use alleycat_codex_proto::command_exec::{
    CommandExecParams, CommandExecResizeResponse, CommandExecResponse,
    CommandExecTerminateResponse, CommandExecWriteResponse,
};
use alleycat_codex_proto::common::{
    ApprovalsReviewer, AskForApproval, ReasoningEffort, SandboxMode, SessionSource, ThreadStatus,
    TurnStatus,
};
use alleycat_codex_proto::config::{
    ConfigReadResponse, ConfigRequirementsReadResponse, ConfigWriteResponse, WriteStatus,
};
use alleycat_codex_proto::items::{ThreadItem, UserInput};
use alleycat_codex_proto::lifecycle::InitializeResponse;
use alleycat_codex_proto::mcp::{
    ListMcpServerStatusResponse, McpServerOauthLoginResponse, McpServerRefreshResponse,
};
use alleycat_codex_proto::model::ModelListResponse;
use alleycat_codex_proto::notifications::{
    AgentMessageDeltaNotification, ItemCompletedNotification, ItemStartedNotification,
    ThreadIdOnly, ThreadNameUpdatedNotification, ThreadStartedNotification,
    TurnCompletedNotification, TurnStartedNotification,
};
use alleycat_codex_proto::skills::{SkillsConfigWriteResponse, SkillsListResponse};
use alleycat_codex_proto::thread::{
    Thread, ThreadArchiveParams, ThreadArchiveResponse, ThreadBackgroundTerminalsCleanResponse,
    ThreadForkParams, ThreadForkResponse, ThreadListParams, ThreadListResponse,
    ThreadLoadedListResponse, ThreadReadParams, ThreadReadResponse, ThreadResumeParams,
    ThreadResumeResponse, ThreadRollbackParams, ThreadSetNameParams, ThreadSetNameResponse,
    ThreadStartParams, ThreadStartResponse, ThreadTurnsListParams, ThreadTurnsListResponse, Turn,
};
use alleycat_codex_proto::turn::{TurnInterruptResponse, TurnStartParams, TurnStartResponse};

use crate::api_client::{CreateRunRequest, DEFAULT_API_KEY_ENV, HermesApiClient};
use crate::config::HermesBridgeConfig;
use crate::event_store::{EventStore, NormalizedHermesEvent};
use crate::health_cache::HealthCache;
use crate::index::{HermesBinding, ThreadIndex};
use crate::run_manager::HermesRunManager;
use crate::run_state::{HermesRunRecord, HermesTurnStatus, RunStore};
use crate::state::{ActiveTurn, TurnState};

fn random_hex(len: usize) -> String {
    use rand::RngCore;
    let mut rng = rand::thread_rng();
    std::iter::repeat_with(|| rng.next_u32() as u8)
        .take(len)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn rpc_error(code: i64, message: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code,
        message: message.into(),
        data: None,
    }
}

fn error_response(code: i64, message: &str) -> Result<Value, JsonRpcError> {
    Err(rpc_error(code, message))
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, JsonRpcError> {
    serde_json::to_value(value).map_err(|e| rpc_error(-32603, format!("serialize response: {e}")))
}

fn default_cwd() -> String {
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "/tmp".to_string())
}

fn user_text(input: &[UserInput]) -> String {
    input
        .iter()
        .filter_map(|inp| match inp {
            UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<&str>>()
        .join("\n")
}

fn preview_for(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(120).collect())
    }
}

fn truncate_string(value: &mut String, cap: usize) {
    if value.len() <= cap {
        return;
    }
    let mut end = cap;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push_str("\n...[truncated]");
}

fn in_progress_turn(turn_id: &str) -> Turn {
    Turn {
        id: turn_id.to_string(),
        items: vec![],
        items_view: "full".to_string(),
        status: TurnStatus::InProgress,
        error: None,
        started_at: Some(epoch_ms()),
        completed_at: None,
        duration_ms: None,
    }
}

fn completed_turn(turn_id: &str) -> Turn {
    let mut turn = in_progress_turn(turn_id);
    turn.status = TurnStatus::Completed;
    turn.completed_at = Some(epoch_ms());
    turn
}

#[derive(Clone)]
pub struct HermesBridge {
    config: HermesBridgeConfig,
    index: Arc<ThreadIndex>,
    state: Arc<TurnState>,
    turns: Arc<Mutex<HashMap<String, Vec<Turn>>>>,
    api_client: Arc<HermesApiClient>,
    run_store: Arc<RunStore>,
    event_store: Arc<EventStore>,
    run_manager: Arc<HermesRunManager>,
    health_cache: Arc<HealthCache>,
}

impl HermesBridge {
    pub fn new(config: HermesBridgeConfig) -> Self {
        let api_base = match &config.mode {
            crate::config::HermesMode::Api { api_base } => api_base.clone(),
            crate::config::HermesMode::Auto { api_base, .. } => api_base.clone(),
            crate::config::HermesMode::Cli { .. } => {
                crate::api_client::DEFAULT_API_BASE.to_string()
            }
        };
        let api_key = std::env::var(DEFAULT_API_KEY_ENV)
            .or_else(|_| std::env::var("API_SERVER_KEY"))
            .ok()
            .or_else(load_api_key_from_dotenv);
        let state_dir = config.state_dir.as_ref().map(PathBuf::from);
        let index = state_dir
            .as_ref()
            .and_then(|dir| ThreadIndex::open_sync(dir.join("threads.json")).ok())
            .unwrap_or_else(ThreadIndex::new_in_memory);
        let run_store = state_dir
            .as_ref()
            .and_then(|dir| RunStore::open_sync(dir.join("runs.json")).ok())
            .unwrap_or_else(RunStore::new_in_memory);
        // Mark any orphaned runs from a prior daemon process as Unknown.
        let _ = run_store.mark_orphans_unknown(epoch_ms());
        let event_store = state_dir
            .as_ref()
            .and_then(|dir| EventStore::open_sync(dir.join("events")).ok())
            .unwrap_or_else(EventStore::new_in_memory);
        let api_client = Arc::new(HermesApiClient::new(&api_base, api_key));
        let run_store = Arc::new(run_store);
        let event_store = Arc::new(event_store);
        let run_manager = Arc::new(HermesRunManager::new(
            Arc::clone(&api_client),
            Arc::clone(&run_store),
            Arc::clone(&event_store),
        ));
        let health_cache = Arc::new(HealthCache::new(config.health_cache_ttl_ms));
        tracing::info!(
            agent = "hermes",
            api_base = %api_base,
            state_dir = ?state_dir,
            mode = ?config.mode,
            "hermes bridge constructed"
        );
        Self {
            config,
            index: Arc::new(index),
            state: Arc::new(TurnState::new()),
            turns: Arc::new(Mutex::new(HashMap::new())),
            api_client,
            run_store,
            event_store,
            run_manager,
            health_cache,
        }
    }

    fn binding_to_thread(binding: &HermesBinding) -> Thread {
        let now = epoch_ms();
        Thread {
            id: binding.thread_id.clone(),
            session_id: binding.hermes_session_id.clone(),
            forked_from_id: binding.forked_from_id.clone(),
            preview: binding.preview.clone().unwrap_or_default(),
            ephemeral: false,
            model_provider: "hermes-agent".to_string(),
            created_at: binding.created_at,
            updated_at: if binding.updated_at == 0 {
                now
            } else {
                binding.updated_at
            },
            status: ThreadStatus::Idle,
            path: None,
            cwd: binding.cwd.clone().unwrap_or_else(default_cwd),
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            source: SessionSource::AppServer,
            thread_source: None,
            agent_nickname: Some("hermes".to_string()),
            agent_role: Some("assistant".to_string()),
            git_info: None,
            name: binding.name.clone(),
            turns: vec![],
        }
    }

    fn start_like_response(thread: Thread, model: Option<String>) -> ThreadResumeResponse {
        let cwd = thread.cwd.clone();
        let model = model.unwrap_or_else(|| "hermes-agent".to_string());
        ThreadResumeResponse {
            thread,
            model,
            model_provider: "hermes-agent".to_string(),
            service_tier: None,
            cwd,
            instruction_sources: vec![],
            approval_policy: AskForApproval::Never,
            approvals_reviewer: ApprovalsReviewer::User,
            sandbox: json!({"type": "dangerFullAccess"}),
            permission_profile: None,
            active_permission_profile: None,
            reasoning_effort: Some(ReasoningEffort::Medium),
        }
    }

    fn persist_index(&self) -> Result<(), JsonRpcError> {
        self.index
            .persist()
            .map_err(|e| rpc_error(-32603, format!("persist thread index: {e}")))
    }

    fn start_logged_turn(&self, thread_id: &str, turn_id: &str) {
        let turn = Turn {
            id: turn_id.to_string(),
            items: vec![],
            items_view: "full".to_string(),
            status: TurnStatus::InProgress,
            error: None,
            started_at: Some(epoch_ms()),
            completed_at: None,
            duration_ms: None,
        };
        self.turns
            .lock()
            .unwrap()
            .entry(thread_id.to_string())
            .or_default()
            .push(turn);
    }

    fn push_logged_item(&self, thread_id: &str, turn_id: &str, item: ThreadItem) {
        if let Some(turn) = self
            .turns
            .lock()
            .unwrap()
            .get_mut(thread_id)
            .and_then(|turns| turns.iter_mut().rev().find(|turn| turn.id == turn_id))
        {
            turn.items.push(item);
        }
    }

    fn complete_logged_turn(&self, thread_id: &str, turn_id: &str, error: Option<String>) {
        if let Some(turn) = self
            .turns
            .lock()
            .unwrap()
            .get_mut(thread_id)
            .and_then(|turns| turns.iter_mut().rev().find(|turn| turn.id == turn_id))
        {
            turn.status = if error.is_some() {
                TurnStatus::Failed
            } else {
                TurnStatus::Completed
            };
            turn.error = error.map(|message| alleycat_codex_proto::common::TurnError {
                message,
                codex_error_info: None,
                additional_details: None,
            });
            turn.completed_at = Some(epoch_ms());
        }
    }

    fn logged_turns(&self, thread_id: &str) -> Vec<Turn> {
        // Two sources, merged: in-memory `self.turns` (live during the current
        // process) and `RunStore` + `EventStore` (durable, survives restart).
        // Prefer the in-memory representation for entries that exist in both,
        // because it carries fully-formed thread items already.
        let mut by_id: std::collections::HashMap<String, Turn> = self
            .turns
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();
        for record in self.run_store.list_for_thread(thread_id) {
            if by_id.contains_key(&record.turn_id) {
                continue;
            }
            by_id.insert(
                record.turn_id.clone(),
                turn_from_run_record(&record, self.event_store.as_ref()),
            );
        }
        let mut merged: Vec<Turn> = by_id.into_values().collect();
        merged.sort_by_key(|turn| turn.started_at.unwrap_or(0));
        merged
    }
}

fn turn_from_run_record(record: &HermesRunRecord, event_store: &EventStore) -> Turn {
    use alleycat_codex_proto::common::{TurnError, TurnStatus};
    let status = match record.status {
        HermesTurnStatus::Completed => TurnStatus::Completed,
        HermesTurnStatus::Failed | HermesTurnStatus::Cancelled => TurnStatus::Failed,
        HermesTurnStatus::Starting | HermesTurnStatus::Running => TurnStatus::InProgress,
        HermesTurnStatus::Unknown => TurnStatus::Failed,
    };
    let mut items: Vec<ThreadItem> = Vec::new();
    if let Some(run_id) = record.run_id.as_deref() {
        if let Ok(events) = event_store.read_all(run_id) {
            for ev in events {
                if ev.method == "item/started" || ev.method == "item/completed" {
                    if let Some(item) = ev.params.get("item").cloned() {
                        if let Ok(parsed) = serde_json::from_value::<ThreadItem>(item) {
                            // Replace prior partial of same id with the
                            // newer (more complete) representation.
                            let id = parsed.id().to_string();
                            if let Some(existing_pos) = items.iter().position(|i| i.id() == id) {
                                items[existing_pos] = parsed;
                            } else {
                                items.push(parsed);
                            }
                        }
                    }
                }
            }
        }
    }
    Turn {
        id: record.turn_id.clone(),
        items,
        items_view: "full".to_string(),
        status,
        error: record.error.clone().map(|message| TurnError {
            message,
            codex_error_info: None,
            additional_details: None,
        }),
        started_at: Some(record.created_at),
        completed_at: record.completed_at,
        duration_ms: record
            .completed_at
            .map(|end| (end - record.created_at).max(0)),
    }
}

#[async_trait]
impl Bridge for HermesBridge {
    async fn initialize(&self, ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        ctx.set_initialize_capabilities(&params);
        let home = directories::ProjectDirs::from("", "", "alleycat")
            .map(|d| d.data_dir().to_string_lossy().to_string())
            .unwrap_or_else(|| "/tmp/alleycat".to_string());
        to_value(InitializeResponse {
            user_agent: format!("hermes-bridge/{}", env!("CARGO_PKG_VERSION")),
            codex_home: home,
            platform_family: "linux".to_string(),
            platform_os: std::env::consts::OS.to_string(),
        })
    }

    async fn dispatch(
        &self,
        ctx: &Conn,
        method: &str,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        match method {
            "thread/start" => self.handle_thread_start(ctx, params).await,
            "thread/resume" => self.handle_thread_resume(ctx, params).await,
            "thread/fork" => self.handle_thread_fork(ctx, params).await,
            "thread/archive" => self.handle_thread_archive(ctx, params).await,
            "thread/unarchive" => self.handle_thread_unarchive(ctx, params).await,
            "thread/name/set" => self.handle_thread_name_set(ctx, params).await,
            "thread/compact/start" => self.handle_thread_compact_start(ctx, params).await,
            "thread/rollback" => self.handle_thread_rollback(ctx, params).await,
            "thread/list" => self.handle_thread_list(ctx, params).await,
            "thread/loaded/list" => self.handle_thread_loaded_list(ctx, params).await,
            "thread/read" => self.handle_thread_read(ctx, params).await,
            "thread/turns/list" => self.handle_thread_turns_list(ctx, params).await,
            "thread/backgroundTerminals/clean" => {
                self.handle_thread_background_terminals_clean(ctx, params)
                    .await
            }
            "turn/start" => self.handle_turn_start(ctx, params).await,
            "turn/steer" => self.handle_turn_steer(ctx, params).await,
            "turn/interrupt" => self.handle_turn_interrupt(ctx, params).await,
            "review/start" => self.handle_review_start(ctx, params).await,
            "model/list" => self.handle_model_list(ctx, params).await,
            "account/read" => self.handle_account_read(ctx, params).await,
            "account/rateLimits/read" => self.handle_account_rate_limits_read(ctx, params).await,
            "account/login/start" => self.handle_account_login_start(ctx, params).await,
            "account/login/cancel" => self.handle_account_login_cancel(ctx, params).await,
            "account/logout" => self.handle_account_logout(ctx, params).await,
            "config/read" => self.handle_config_read(ctx, params).await,
            "config/value/write" => self.handle_config_value_write(ctx, params).await,
            "config/batchWrite" => self.handle_config_batch_write(ctx, params).await,
            "configRequirements/read" => self.handle_config_requirements_read(ctx, params).await,
            "mcpServerStatus/list" => self.handle_mcp_server_status_list(ctx, params).await,
            "config/mcpServer/reload" => self.handle_config_mcp_server_reload(ctx, params).await,
            "mcpServer/oauth/login" => self.handle_mcp_server_oauth_login(ctx, params).await,
            "skills/list" => self.handle_skills_list(ctx, params).await,
            "skills/remote/list" => self.handle_skills_remote_list(ctx, params).await,
            "skills/remote/export" => self.handle_skills_remote_export(ctx, params).await,
            "skills/config/write" => self.handle_skills_config_write(ctx, params).await,
            "command/exec" => self.handle_command_exec(ctx, params).await,
            "command/exec/write" => self.handle_command_exec_write(ctx, params).await,
            "command/exec/terminate" => self.handle_command_exec_terminate(ctx, params).await,
            "command/exec/resize" => self.handle_command_exec_resize(ctx, params).await,
            "mock/experimentalMethod" => self.handle_mock_experimental_method(ctx, params).await,
            "experimentalFeature/list" => self.handle_experimental_feature_list(ctx, params).await,
            "collaborationMode/list" => self.handle_collaboration_mode_list(ctx, params).await,
            "feedback/upload" => self.handle_feedback_upload(ctx, params).await,
            _ => error_response(-32601, &format!("Method not found: {method}")),
        }
    }

    async fn notification(&self, _ctx: &Conn, _method: &str, _params: Value) {
        // No client-originated notifications currently require bridge-side state changes.
    }
}

impl HermesBridge {
    async fn handle_thread_start(&self, ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let p: ThreadStartParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        let now = epoch_ms();
        let thread_id = format!("thread_{}", random_hex(12));
        let session_id = format!("ses_{}", random_hex(12));
        let cwd = p.cwd.clone().unwrap_or_else(default_cwd);
        let binding = HermesBinding {
            thread_id: thread_id.clone(),
            hermes_session_id: session_id,
            model: p.model.clone(),
            created_at: now,
            updated_at: now,
            preview: None,
            cwd: Some(cwd.clone()),
            name: None,
            forked_from_id: None,
            archived: false,
        };
        self.index.upsert(binding.clone());
        self.persist_index()?;
        let thread = Self::binding_to_thread(&binding);
        let _ = ctx.notifier().send_notification(
            "thread/started",
            ThreadStartedNotification {
                thread: thread.clone(),
            },
        );
        let base = Self::start_like_response(thread, p.model.clone());
        to_value(ThreadStartResponse {
            thread: base.thread,
            model: base.model,
            model_provider: base.model_provider,
            service_tier: base.service_tier,
            cwd,
            instruction_sources: base.instruction_sources,
            approval_policy: p.approval_policy.unwrap_or(AskForApproval::Never),
            approvals_reviewer: p.approvals_reviewer.unwrap_or(ApprovalsReviewer::User),
            sandbox: p
                .sandbox
                .map(|mode| match mode {
                    SandboxMode::ReadOnly => json!({"type": "readOnly"}),
                    SandboxMode::WorkspaceWrite => json!({"type": "workspaceWrite"}),
                    SandboxMode::DangerFullAccess => json!({"type": "dangerFullAccess"}),
                })
                .unwrap_or_else(|| json!({"type": "dangerFullAccess"})),
            permission_profile: p.permission_profile,
            active_permission_profile: None,
            reasoning_effort: Some(ReasoningEffort::Medium),
        })
    }

    async fn handle_thread_resume(&self, ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let p: ThreadResumeParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        let Some(mut binding) = self.index.get_by_thread(&p.thread_id) else {
            return error_response(-32602, "thread not found");
        };
        if let Some(cwd) = p.cwd.clone() {
            binding.cwd = Some(cwd);
            binding.updated_at = epoch_ms();
            self.index.upsert(binding.clone());
            self.persist_index()?;
        }
        let mut thread = Self::binding_to_thread(&binding);
        if !p.exclude_turns {
            thread.turns = self.logged_turns(&p.thread_id);
        }
        let _ = ctx.notifier().send_notification(
            "thread/started",
            ThreadStartedNotification {
                thread: thread.clone(),
            },
        );
        to_value(Self::start_like_response(thread, p.model.or(binding.model)))
    }

    async fn handle_thread_fork(&self, ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let p: ThreadForkParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        let Some(parent) = self.index.get_by_thread(&p.thread_id) else {
            return error_response(-32602, "thread not found");
        };
        let now = epoch_ms();
        let thread_id = format!("thread_{}", random_hex(12));
        let binding = HermesBinding {
            thread_id: thread_id.clone(),
            hermes_session_id: parent.hermes_session_id.clone(),
            model: p.model.clone().or(parent.model.clone()),
            created_at: now,
            updated_at: now,
            preview: parent.preview.clone(),
            cwd: p.cwd.clone().or(parent.cwd.clone()),
            name: parent.name.clone().map(|name| format!("{name} (fork)")),
            forked_from_id: Some(parent.thread_id.clone()),
            archived: false,
        };
        self.index.upsert(binding.clone());
        self.persist_index()?;
        let thread = Self::binding_to_thread(&binding);
        let _ = ctx.notifier().send_notification(
            "thread/started",
            ThreadStartedNotification {
                thread: thread.clone(),
            },
        );
        to_value(ThreadForkResponse {
            ..Self::start_like_response(thread, binding.model)
        })
    }

    async fn handle_thread_archive(
        &self,
        ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let p: ThreadArchiveParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        if self
            .index
            .set_archived(&p.thread_id, true, epoch_ms())
            .is_none()
        {
            return error_response(-32602, "thread not found");
        }
        self.persist_index()?;
        let _ = ctx.notifier().send_notification(
            "thread/archived",
            ThreadIdOnly {
                thread_id: p.thread_id,
            },
        );
        to_value(ThreadArchiveResponse::default())
    }

    async fn handle_thread_unarchive(
        &self,
        ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let p: alleycat_codex_proto::thread::ThreadUnarchiveParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        let Some(binding) = self.index.set_archived(&p.thread_id, false, epoch_ms()) else {
            return error_response(-32602, "thread not found");
        };
        self.persist_index()?;
        let _ = ctx.notifier().send_notification(
            "thread/unarchived",
            ThreadIdOnly {
                thread_id: p.thread_id.clone(),
            },
        );
        to_value(alleycat_codex_proto::thread::ThreadUnarchiveResponse {
            thread: Self::binding_to_thread(&binding),
        })
    }

    async fn handle_thread_name_set(
        &self,
        ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let p: ThreadSetNameParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        if self
            .index
            .set_name(&p.thread_id, Some(p.name.clone()), epoch_ms())
            .is_none()
        {
            return error_response(-32602, "thread not found");
        }
        self.persist_index()?;
        let _ = ctx.notifier().send_notification(
            "thread/name/updated",
            ThreadNameUpdatedNotification {
                thread_id: p.thread_id,
                thread_name: Some(p.name),
            },
        );
        to_value(ThreadSetNameResponse::default())
    }

    async fn handle_thread_compact_start(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        error_response(
            -32601,
            "thread/compact/start is not supported by Hermes bridge",
        )
    }

    async fn handle_thread_rollback(
        &self,
        _ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let _p: ThreadRollbackParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        error_response(-32601, "thread/rollback is not supported by Hermes bridge")
    }

    async fn handle_thread_list(&self, _ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let p: ThreadListParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        let mut bindings = self.index.all();
        if let Some(archived) = p.archived {
            bindings.retain(|binding| binding.archived == archived);
        } else {
            bindings.retain(|binding| !binding.archived);
        }
        if let Some(term) = p.search_term.as_deref().filter(|s| !s.is_empty()) {
            let needle = term.to_lowercase();
            bindings.retain(|binding| {
                binding
                    .preview
                    .as_deref()
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&needle)
                    || binding
                        .name
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&needle)
            });
        }
        let threads = bindings.iter().map(Self::binding_to_thread).collect();
        to_value(ThreadListResponse {
            data: threads,
            next_cursor: None,
            backwards_cursor: None,
        })
    }

    async fn handle_thread_loaded_list(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(ThreadLoadedListResponse {
            data: self.index.thread_ids(),
            next_cursor: None,
        })
    }

    async fn handle_thread_read(&self, _ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let p: ThreadReadParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        match self.index.get_by_thread(&p.thread_id) {
            Some(binding) => {
                let mut thread = Self::binding_to_thread(&binding);
                if p.include_turns {
                    thread.turns = self.logged_turns(&p.thread_id);
                }
                to_value(ThreadReadResponse { thread })
            }
            None => error_response(-32602, "thread not found"),
        }
    }

    async fn handle_thread_turns_list(
        &self,
        _ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let p: ThreadTurnsListParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        if self.index.get_by_thread(&p.thread_id).is_none() {
            return error_response(-32602, "thread not found");
        }
        to_value(ThreadTurnsListResponse {
            data: self.logged_turns(&p.thread_id),
            next_cursor: None,
            backwards_cursor: None,
        })
    }

    async fn handle_thread_background_terminals_clean(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(ThreadBackgroundTerminalsCleanResponse::default())
    }
}

impl HermesBridge {
    async fn handle_turn_start(&self, ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let p: TurnStartParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        let thread_id = p.thread_id.clone();
        let turn_id = format!("turn_{}", random_hex(12));
        let binding = self
            .index
            .get_by_thread(&thread_id)
            .ok_or_else(|| rpc_error(-32602, "thread not found"))?;
        let session_id = binding.hermes_session_id.clone();
        self.start_logged_turn(&thread_id, &turn_id);
        self.state.insert(
            thread_id.clone(),
            ActiveTurn {
                turn_id: turn_id.clone(),
                thread_id: thread_id.clone(),
                hermes_session_id: session_id.clone(),
                run_id: None,
            },
        );
        let turn = Turn {
            id: turn_id.clone(),
            items: vec![],
            items_view: "full".to_string(),
            status: TurnStatus::InProgress,
            error: None,
            started_at: Some(epoch_ms()),
            completed_at: None,
            duration_ms: None,
        };
        let _ = ctx.notifier().send_notification(
            "turn/started",
            TurnStartedNotification {
                thread_id: thread_id.clone(),
                turn,
            },
        );
        self.emit_user_message(ctx, &thread_id, &turn_id, &p.input)
            .await;
        let text = user_text(&p.input);
        let cwd = p.cwd.as_ref().map(|p| p.to_string_lossy().to_string());
        self.index.update_after_turn(
            &thread_id,
            None,
            p.model.clone(),
            preview_for(&text),
            cwd,
            epoch_ms(),
        );
        self.persist_index()?;

        // Persist `Starting` *before* hitting the API. Phase 2.2: durable
        // state must precede streaming so reconnects never see a turn that
        // exists in memory only.
        let now = epoch_ms();
        let agent_item_id = format!("item_{}", random_hex(8));
        let starting_record = HermesRunRecord {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            hermes_session_id: session_id.clone(),
            run_id: None,
            status: HermesTurnStatus::Starting,
            created_at: now,
            updated_at: now,
            completed_at: None,
            error: None,
            accumulated_text: String::new(),
            last_event_seq: None,
            agent_item_id: Some(agent_item_id.clone()),
        };
        if let Err(err) = self.run_store.upsert(starting_record) {
            tracing::warn!(error = %err, turn_id = %turn_id, "persist starting run record failed");
        }

        match &self.config.mode {
            crate::config::HermesMode::Api { .. } => {
                self.dispatch_turn_api(ctx, &thread_id, &turn_id, &session_id, &agent_item_id, &p)
                    .await
            }
            crate::config::HermesMode::Auto { .. } => {
                let snapshot = self
                    .health_cache
                    .get_or_probe(&self.api_client, self.config.health_timeout_ms)
                    .await;
                if snapshot.healthy {
                    self.dispatch_turn_api(
                        ctx,
                        &thread_id,
                        &turn_id,
                        &session_id,
                        &agent_item_id,
                        &p,
                    )
                    .await
                } else {
                    tracing::info!(
                        agent = "hermes",
                        reason = ?snapshot.reason,
                        "hermes auto-mode falling back to CLI"
                    );
                    self.dispatch_turn_cli(ctx, &thread_id, &turn_id, &p).await
                }
            }
            crate::config::HermesMode::Cli { .. } => {
                self.dispatch_turn_cli(ctx, &thread_id, &turn_id, &p).await
            }
        }
    }

    async fn handle_turn_steer(&self, _ctx: &Conn, _params: Value) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_turn_interrupt(
        &self,
        _ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let p: alleycat_codex_proto::turn::TurnInterruptParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        let thread_id = p.thread_id;
        if let Some(active) = self
            .state
            .remove(&thread_id)
            .and_then(|active| active.run_id)
        {
            if let Err(err) = self.run_manager.stop(&active).await {
                tracing::warn!(error = %err, run_id = %active, "hermes stop_run failed");
            }
            // The SSE pump will observe `run.cancelled` and finalize via the
            // normal path. We do not emit terminal frames ourselves here
            // because doing so would race the pump's normalized events.
        }
        to_value(TurnInterruptResponse::default())
    }

    async fn handle_review_start(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        error_response(
            -32601,
            "review/start is not supported by the Hermes backend",
        )
    }
}

impl HermesBridge {
    async fn handle_model_list(&self, _ctx: &Conn, _params: Value) -> Result<Value, JsonRpcError> {
        to_value(ModelListResponse {
            data: vec![alleycat_codex_proto::model::Model {
                id: "hermes-agent".to_string(),
                model: "hermes-agent".to_string(),
                upgrade: None,
                upgrade_info: None,
                availability_nux: None,
                display_name: "Hermes Agent".to_string(),
                description: "Hermes Agent via Alleycat bridge".to_string(),
                hidden: false,
                supported_reasoning_efforts: vec![
                    alleycat_codex_proto::model::ReasoningEffortOption {
                        reasoning_effort: ReasoningEffort::Medium,
                        description: "Default Hermes reasoning effort".to_string(),
                    },
                ],
                default_reasoning_effort: ReasoningEffort::Medium,
                input_modalities: vec![json!("text")],
                supports_personality: false,
                additional_speed_tiers: vec![],
                service_tiers: vec![],
                is_default: true,
            }],
            next_cursor: None,
        })
    }

    async fn handle_account_read(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(GetAccountResponse {
            account: Some(Account::ApiKey {}),
            requires_openai_auth: false,
        })
    }

    async fn handle_account_rate_limits_read(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(GetAccountRateLimitsResponse::default())
    }

    async fn handle_account_login_start(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(LoginAccountResponse::ApiKey {})
    }

    async fn handle_account_login_cancel(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(CancelLoginAccountResponse {
            status: CancelLoginAccountStatus::NotFound,
        })
    }

    async fn handle_account_logout(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(LogoutAccountResponse::default())
    }

    async fn handle_config_read(&self, _ctx: &Conn, _params: Value) -> Result<Value, JsonRpcError> {
        to_value(ConfigReadResponse {
            config: json!({"model_provider": "hermes-agent", "model": "hermes-agent"}),
            origins: Default::default(),
            layers: None,
        })
    }

    async fn handle_config_value_write(
        &self,
        _ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let file_path = params
            .get("filePath")
            .and_then(Value::as_str)
            .unwrap_or("hermes-bridge")
            .to_string();
        to_value(ConfigWriteResponse {
            status: WriteStatus::Ok,
            version: epoch_ms().to_string(),
            file_path,
            overridden_metadata: None,
        })
    }

    async fn handle_config_batch_write(
        &self,
        _ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let file_path = params
            .get("filePath")
            .and_then(Value::as_str)
            .unwrap_or("hermes-bridge")
            .to_string();
        to_value(ConfigWriteResponse {
            status: WriteStatus::Ok,
            version: epoch_ms().to_string(),
            file_path,
            overridden_metadata: None,
        })
    }

    async fn handle_config_requirements_read(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(ConfigRequirementsReadResponse::default())
    }

    async fn handle_mcp_server_status_list(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(ListMcpServerStatusResponse::default())
    }

    async fn handle_config_mcp_server_reload(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(McpServerRefreshResponse::default())
    }

    async fn handle_mcp_server_oauth_login(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(McpServerOauthLoginResponse {
            authorization_url: String::new(),
        })
    }

    async fn handle_skills_list(&self, _ctx: &Conn, _params: Value) -> Result<Value, JsonRpcError> {
        to_value(SkillsListResponse { data: vec![] })
    }

    async fn handle_skills_remote_list(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(SkillsListResponse { data: vec![] })
    }

    async fn handle_skills_remote_export(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_skills_config_write(
        &self,
        _ctx: &Conn,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(SkillsConfigWriteResponse {
            effective_enabled: params
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        })
    }

    async fn handle_command_exec(&self, _ctx: &Conn, params: Value) -> Result<Value, JsonRpcError> {
        let p: CommandExecParams = serde_json::from_value(params)
            .map_err(|e| rpc_error(-32602, format!("Invalid params: {e}")))?;
        if p.command.is_empty() {
            return error_response(-32602, "command/exec requires a non-empty command");
        }
        if p.tty || p.stream_stdin || p.stream_stdout_stderr || p.process_id.is_some() {
            return error_response(
                -32602,
                "command/exec streaming and TTY modes are not supported by the Hermes bridge; use buffered exec",
            );
        }

        let mut cmd = Command::new(&p.command[0]);
        cmd.args(&p.command[1..]);
        if let Some(cwd) = p.cwd {
            cmd.current_dir(cwd);
        }
        if let Some(env) = p.env {
            for (key, value) in env {
                match value {
                    Some(value) => {
                        cmd.env(key, value);
                    }
                    None => {
                        cmd.env_remove(key);
                    }
                }
            }
        }

        let timeout_ms = if p.disable_timeout {
            None
        } else {
            Some(p.timeout_ms.unwrap_or(30_000).clamp(1, 300_000) as u64)
        };
        let output_result = if let Some(timeout_ms) = timeout_ms {
            tokio::time::timeout(Duration::from_millis(timeout_ms), cmd.output())
                .await
                .map_err(|_| rpc_error(-32603, "command/exec timed out"))?
        } else {
            cmd.output().await
        };
        let output = output_result
            .map_err(|e| rpc_error(-32603, format!("command/exec failed to spawn: {e}")))?;
        let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !p.disable_output_cap {
            let cap = p.output_bytes_cap.unwrap_or(1_048_576);
            truncate_string(&mut stdout, cap);
            truncate_string(&mut stderr, cap);
        }
        to_value(CommandExecResponse {
            exit_code: output.status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    }

    async fn handle_command_exec_write(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(CommandExecWriteResponse::default())
    }

    async fn handle_command_exec_terminate(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(CommandExecTerminateResponse::default())
    }

    async fn handle_command_exec_resize(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(CommandExecResizeResponse::default())
    }

    async fn handle_mock_experimental_method(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({}))
    }

    async fn handle_experimental_feature_list(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({ "data": [], "nextCursor": null }))
    }

    async fn handle_collaboration_mode_list(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        Ok(json!({ "data": [] }))
    }

    async fn handle_feedback_upload(
        &self,
        _ctx: &Conn,
        _params: Value,
    ) -> Result<Value, JsonRpcError> {
        to_value(alleycat_codex_proto::account::FeedbackUploadResponse::default())
    }
}

impl HermesBridge {
    async fn dispatch_turn_api(
        &self,
        ctx: &Conn,
        thread_id: &str,
        turn_id: &str,
        session_id: &str,
        agent_item_id: &str,
        params: &TurnStartParams,
    ) -> Result<Value, JsonRpcError> {
        let text = user_text(&params.input);
        let cwd = params
            .cwd
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .or_else(|| {
                self.index
                    .get_by_thread(thread_id)
                    .and_then(|binding| binding.cwd)
            });
        let request = CreateRunRequest {
            input: text,
            session_id: Some(session_id.to_string()),
            cwd,
            model: params.model.clone(),
        };
        let run = match self.api_client.create_run(request).await {
            Ok(run) => run,
            Err(e) => {
                let message = format!("Hermes API error: {e}");
                let now = epoch_ms();
                let _ = self.run_store.mark_terminal(
                    turn_id,
                    HermesTurnStatus::Failed,
                    Some(message.clone()),
                    None,
                    now,
                );
                self.emit_turn_completed(ctx, thread_id, turn_id, None, Some(message.clone()))
                    .await;
                return error_response(-32603, &message);
            }
        };
        if let Some(ref hermes_session_id) = run.session_id {
            self.index.update_after_turn(
                thread_id,
                Some(hermes_session_id.clone()),
                params.model.clone(),
                None,
                None,
                epoch_ms(),
            );
            self.persist_index()?;
        }
        self.state.insert(
            thread_id.to_string(),
            ActiveTurn {
                turn_id: turn_id.to_string(),
                thread_id: thread_id.to_string(),
                hermes_session_id: session_id.to_string(),
                run_id: Some(run.run_id.clone()),
            },
        );

        // Persist Running state with the run_id so reconnects can discover
        // the active run.
        let updated = match self
            .run_store
            .mark_running(turn_id, &run.run_id, epoch_ms())
        {
            Ok(Some(record)) => record,
            Ok(None) => {
                tracing::warn!(turn_id = %turn_id, "mark_running: no record found; reconstructing");
                HermesRunRecord {
                    thread_id: thread_id.to_string(),
                    turn_id: turn_id.to_string(),
                    hermes_session_id: session_id.to_string(),
                    run_id: Some(run.run_id.clone()),
                    status: HermesTurnStatus::Running,
                    created_at: epoch_ms(),
                    updated_at: epoch_ms(),
                    completed_at: None,
                    error: None,
                    accumulated_text: String::new(),
                    last_event_seq: None,
                    agent_item_id: Some(agent_item_id.to_string()),
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, turn_id = %turn_id, "mark_running failed");
                HermesRunRecord {
                    thread_id: thread_id.to_string(),
                    turn_id: turn_id.to_string(),
                    hermes_session_id: session_id.to_string(),
                    run_id: Some(run.run_id.clone()),
                    status: HermesTurnStatus::Running,
                    created_at: epoch_ms(),
                    updated_at: epoch_ms(),
                    completed_at: None,
                    error: None,
                    accumulated_text: String::new(),
                    last_event_seq: None,
                    agent_item_id: Some(agent_item_id.to_string()),
                }
            }
        };

        let auto_approve = matches!(params.approval_policy.as_ref(), Some(AskForApproval::Never));

        // The run manager owns the single SSE pump. Per-Conn translators
        // subscribe to a broadcast and forward normalized events as JSON-RPC
        // notifications. This `Conn`'s subscription replays nothing because
        // we just created the run.
        self.run_manager
            .ensure_run(updated, agent_item_id.to_string(), auto_approve);
        self.spawn_translator(ctx.clone(), run.run_id.clone(), 0);

        let response_turn = in_progress_turn(turn_id);
        to_value(TurnStartResponse {
            turn: response_turn,
        })
    }

    /// Spawn a per-`Conn` task that forwards normalized run events as
    /// JSON-RPC notifications. Replays any persisted events with seq >
    /// `after_seq` first. Exits when the broadcast channel closes (the run
    /// is terminal) or the connection's notifier rejects (drainer gone).
    fn spawn_translator(&self, ctx: Conn, run_id: String, after_seq: u64) {
        let subscription = self.run_manager.subscribe(&run_id, after_seq);
        let manager = Arc::clone(&self.run_manager);
        tokio::spawn(async move {
            translator_loop(ctx, manager, run_id, subscription).await;
        });
    }

    async fn dispatch_turn_cli(
        &self,
        ctx: &Conn,
        thread_id: &str,
        turn_id: &str,
        params: &TurnStartParams,
    ) -> Result<Value, JsonRpcError> {
        let text = user_text(&params.input);
        let binding = self.index.get_by_thread(thread_id);
        let (bin, session_id) = match &self.config.mode {
            crate::config::HermesMode::Cli { bin }
            | crate::config::HermesMode::Auto { bin, .. } => (
                bin.clone().unwrap_or_else(|| "hermes".to_string()),
                binding.as_ref().map(|b| b.hermes_session_id.clone()),
            ),
            crate::config::HermesMode::Api { .. } => ("hermes".to_string(), None),
        };
        let cwd = params
            .cwd
            .clone()
            .or_else(|| binding.and_then(|b| b.cwd.map(PathBuf::from)));
        tracing::info!(
            agent = "hermes",
            thread_id = %thread_id,
            turn_id = %turn_id,
            bin = %bin,
            session_id = ?session_id,
            "hermes CLI fallback dispatch (synthetic single-completion path)"
        );
        let started = std::time::Instant::now();
        let result =
            crate::cli_adapter::run_hermes_cli(&bin, &text, session_id.as_deref(), cwd.as_ref())
                .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(output) => {
                tracing::info!(
                    agent = "hermes",
                    thread_id = %thread_id,
                    turn_id = %turn_id,
                    bin = %bin,
                    elapsed_ms,
                    output_chars = output.chars().count(),
                    "hermes CLI fallback completed"
                );
                // Persist a terminal record so post-restart `thread/read`
                // can show the CLI-fallback turn.
                let _ = self.run_store.mark_terminal(
                    turn_id,
                    HermesTurnStatus::Completed,
                    None,
                    Some(output.clone()),
                    epoch_ms(),
                );
                self.emit_synthetic_completion(ctx, thread_id, turn_id, &output)
                    .await;
                to_value(TurnStartResponse {
                    turn: completed_turn(turn_id),
                })
            }
            Err(e) => {
                let message = format!("Hermes CLI error: {e}");
                tracing::warn!(
                    agent = "hermes",
                    thread_id = %thread_id,
                    turn_id = %turn_id,
                    bin = %bin,
                    elapsed_ms,
                    error = %e,
                    "hermes CLI fallback failed"
                );
                let _ = self.run_store.mark_terminal(
                    turn_id,
                    HermesTurnStatus::Failed,
                    Some(message.clone()),
                    None,
                    epoch_ms(),
                );
                self.emit_turn_completed(ctx, thread_id, turn_id, None, Some(message.clone()))
                    .await;
                error_response(-32603, &message)
            }
        }
    }

    async fn emit_user_message(
        &self,
        ctx: &Conn,
        thread_id: &str,
        turn_id: &str,
        input: &[UserInput],
    ) {
        let item_id = format!("item_{}", random_hex(8));
        let item = ThreadItem::UserMessage {
            id: item_id,
            content: input.to_vec(),
        };
        self.push_logged_item(thread_id, turn_id, item.clone());
        let _ = ctx.notifier().send_notification(
            "item/started",
            ItemStartedNotification {
                item: item.clone(),
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                parent_item_id: None,
            },
        );
        let _ = ctx.notifier().send_notification(
            "item/completed",
            ItemCompletedNotification {
                item,
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                parent_item_id: None,
            },
        );
    }

    async fn emit_agent_started(&self, ctx: &Conn, thread_id: &str, turn_id: &str, item_id: &str) {
        let item = ThreadItem::AgentMessage {
            id: item_id.to_string(),
            text: String::new(),
            phase: None,
            memory_citation: None,
        };
        let _ = ctx.notifier().send_notification(
            "item/started",
            ItemStartedNotification {
                item,
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                parent_item_id: None,
            },
        );
    }

    async fn emit_agent_delta(
        &self,
        ctx: &Conn,
        thread_id: &str,
        turn_id: &str,
        item_id: &str,
        delta: &str,
    ) {
        let _ = ctx.notifier().send_notification(
            "item/agentMessage/delta",
            AgentMessageDeltaNotification {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                item_id: item_id.to_string(),
                delta: delta.to_string(),
                parent_item_id: None,
            },
        );
    }

    async fn emit_agent_completed(
        &self,
        ctx: &Conn,
        thread_id: &str,
        turn_id: &str,
        item_id: &str,
        text: &str,
    ) {
        let item = ThreadItem::AgentMessage {
            id: item_id.to_string(),
            text: text.to_string(),
            phase: None,
            memory_citation: None,
        };
        self.push_logged_item(thread_id, turn_id, item.clone());
        let _ = ctx.notifier().send_notification(
            "item/completed",
            ItemCompletedNotification {
                item,
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                parent_item_id: None,
            },
        );
    }

    async fn emit_turn_completed(
        &self,
        ctx: &Conn,
        thread_id: &str,
        turn_id: &str,
        _text: Option<&str>,
        error: Option<String>,
    ) {
        let status = if error.is_some() {
            TurnStatus::Failed
        } else {
            TurnStatus::Completed
        };
        let turn = Turn {
            id: turn_id.to_string(),
            items: vec![],
            items_view: "full".to_string(),
            status,
            error: error
                .clone()
                .map(|message| alleycat_codex_proto::common::TurnError {
                    message,
                    codex_error_info: None,
                    additional_details: None,
                }),
            started_at: Some(epoch_ms()),
            completed_at: Some(epoch_ms()),
            duration_ms: None,
        };
        self.complete_logged_turn(thread_id, turn_id, error);
        let _ = ctx.notifier().send_notification(
            "turn/completed",
            TurnCompletedNotification {
                thread_id: thread_id.to_string(),
                turn,
            },
        );
        self.state.remove(thread_id);
    }

    async fn emit_synthetic_completion(
        &self,
        ctx: &Conn,
        thread_id: &str,
        turn_id: &str,
        text: &str,
    ) {
        let item_id = format!("item_{}", random_hex(8));
        self.emit_agent_started(ctx, thread_id, turn_id, &item_id)
            .await;
        self.emit_agent_delta(ctx, thread_id, turn_id, &item_id, text)
            .await;
        self.emit_agent_completed(ctx, thread_id, turn_id, &item_id, text)
            .await;
        self.emit_turn_completed(ctx, thread_id, turn_id, Some(text), None)
            .await;
    }
}

/// Per-`Conn` translator: pumps normalized run events from the manager's
/// broadcast into JSON-RPC notifications on the connection. Replays any
/// persisted events with `seq > after_seq` first, then live-tails the
/// broadcast until it closes (terminal) or the receiver lags.
async fn translator_loop(
    ctx: Conn,
    manager: Arc<HermesRunManager>,
    run_id: String,
    mut subscription: crate::run_manager::RunSubscription,
) {
    // Replay persisted events first. Note: replay does NOT re-prompt the
    // client for approval requests — those are stateful interactions and
    // are forwarded as plain notifications during replay so reconnecting
    // clients can show the historical event but the request/response
    // contract is owned by the original prompt (or the in-flight one).
    let mut last_seq: u64 = 0;
    for event in subscription.replay.drain(..) {
        last_seq = last_seq.max(event.seq);
        if let Err(err) = forward_event(&ctx, &event) {
            tracing::debug!(run_id = %run_id, error = ?err, "hermes translator: notifier closed during replay");
            return;
        }
    }
    // Live-tail.
    loop {
        match subscription.rx.recv().await {
            Ok(event) => {
                // Skip duplicates that came in between replay snapshot and
                // first live frame (best-effort).
                if event.seq <= last_seq {
                    continue;
                }
                last_seq = event.seq;
                if event.method == "hermes/approvalRequest" {
                    handle_approval_request(&ctx, &manager, &run_id, &event).await;
                    continue;
                }
                if let Err(err) = forward_event(&ctx, &event) {
                    tracing::debug!(run_id = %run_id, error = ?err, "hermes translator: notifier closed");
                    return;
                }
            }
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    run_id = %run_id,
                    skipped,
                    "hermes translator: broadcast lagged; subscriber lost frames"
                );
                // Continue; subsequent reads from broadcast will return the
                // newest available frames. The persisted EventStore is the
                // authoritative replay source if a fresh reattach happens.
                continue;
            }
            Err(RecvError::Closed) => {
                tracing::debug!(run_id = %run_id, "hermes translator: broadcast closed (run terminal)");
                return;
            }
        }
    }
}

fn forward_event(ctx: &Conn, event: &NormalizedHermesEvent) -> Result<(), JsonRpcError> {
    ctx.notifier()
        .send_notification(event.method.clone(), event.params.clone())
        .map_err(|err| rpc_error(-32603, format!("forward event: {err}")))
}

/// Maximum time to wait for the user to answer an approval prompt before
/// giving up. The session machinery still replays the outstanding request to
/// any reattaching client during this window, so a brief network drop does
/// not auto-deny.
const APPROVAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

async fn handle_approval_request(
    ctx: &Conn,
    manager: &Arc<HermesRunManager>,
    run_id: &str,
    event: &NormalizedHermesEvent,
) {
    // Forward the same `hermes/approvalRequest` as a server→client request.
    // The session pending/outstanding tables ensure reattach within the
    // pending grace replays the prompt, so a flaky client can still answer.
    let response = match ctx
        .notifier()
        .request(
            event.method.clone(),
            event.params.clone(),
            APPROVAL_REQUEST_TIMEOUT,
        )
        .await
    {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(run_id = %run_id, error = ?err, "hermes approval request failed; defaulting to deny");
            // Fall back to deny if no client could answer.
            let _ = manager
                .submit_approval(run_id, crate::api_client::ApprovalChoice::Deny, false)
                .await;
            return;
        }
    };
    let choice = parse_approval_choice(&response).unwrap_or_else(|| {
        tracing::warn!(run_id = %run_id, response = ?response, "hermes approval response unparseable; defaulting to deny");
        crate::api_client::ApprovalChoice::Deny
    });
    let resolve_all = response
        .get("resolveAll")
        .or_else(|| response.get("resolve_all"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Err(err) = manager.submit_approval(run_id, choice, resolve_all).await {
        tracing::warn!(run_id = %run_id, error = %err, choice = choice.as_str(), "hermes approval submit failed");
    }
}

fn parse_approval_choice(value: &Value) -> Option<crate::api_client::ApprovalChoice> {
    use crate::api_client::ApprovalChoice;
    let raw = value.get("choice").and_then(Value::as_str)?;
    let lowered = raw.trim().to_ascii_lowercase();
    Some(match lowered.as_str() {
        "once" | "approve" | "approved" | "allow" => ApprovalChoice::Once,
        "session" => ApprovalChoice::Session,
        "always" => ApprovalChoice::Always,
        "deny" | "reject" | "denied" => ApprovalChoice::Deny,
        _ => return None,
    })
}

/// Best-effort fallback: read `API_SERVER_KEY=` from `~/.hermes/.env` when
/// no env var is set. Silent on any error — the env-var path is the primary.
fn load_api_key_from_dotenv() -> Option<String> {
    let dotenv = directories::BaseDirs::new()?
        .home_dir()
        .join(".hermes/.env");
    let text = std::fs::read_to_string(dotenv).ok()?;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("API_SERVER_KEY=") {
            let value = rest.trim_matches(|c: char| c == '"' || c == '\'').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}
