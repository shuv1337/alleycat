use std::time::{SystemTime, UNIX_EPOCH};

use alleycat_codex_proto::notifications::{
    RemoteControlConnectionStatus, RemoteControlStatusChangedNotification,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexRemoteControlState {
    Idle,
    Connecting,
    Disabled,
    Connected,
    Errored,
    Blocked,
    Stopped,
}

impl From<RemoteControlConnectionStatus> for CodexRemoteControlState {
    fn from(value: RemoteControlConnectionStatus) -> Self {
        match value {
            RemoteControlConnectionStatus::Disabled => Self::Disabled,
            RemoteControlConnectionStatus::Connecting => Self::Connecting,
            RemoteControlConnectionStatus::Connected => Self::Connected,
            RemoteControlConnectionStatus::Errored => Self::Errored,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRemoteControlSnapshot {
    pub state: CodexRemoteControlState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_enable_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

impl Default for CodexRemoteControlSnapshot {
    fn default() -> Self {
        Self {
            state: CodexRemoteControlState::Idle,
            server_name: None,
            environment_id: None,
            last_update_unix_ms: None,
            last_enable_reason: None,
            error: None,
            blocked: None,
        }
    }
}

impl CodexRemoteControlSnapshot {
    pub(crate) fn connecting() -> Self {
        Self {
            state: CodexRemoteControlState::Connecting,
            last_update_unix_ms: Some(now_unix_ms()),
            ..Self::default()
        }
    }

    pub(crate) fn stopped() -> Self {
        Self {
            state: CodexRemoteControlState::Stopped,
            last_update_unix_ms: Some(now_unix_ms()),
            ..Self::default()
        }
    }

    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self {
            state: CodexRemoteControlState::Errored,
            last_update_unix_ms: Some(now_unix_ms()),
            error: Some(message.into()),
            ..Self::default()
        }
    }

    pub(crate) fn blocked(message: impl Into<String>) -> Self {
        Self {
            state: CodexRemoteControlState::Blocked,
            last_update_unix_ms: Some(now_unix_ms()),
            blocked: Some(message.into()),
            ..Self::default()
        }
    }

    pub(crate) fn apply_status_changed(&mut self, params: RemoteControlStatusChangedNotification) {
        self.state = params.status.into();
        self.server_name = params.server_name;
        self.environment_id = params.environment_id;
        self.last_update_unix_ms = Some(now_unix_ms());
        self.error = None;
        self.blocked = None;
    }
}

pub(crate) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
