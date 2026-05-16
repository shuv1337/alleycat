//! Hermes bridge configuration.

use serde::{Deserialize, Serialize};

use crate::api_client::DEFAULT_API_BASE;

/// Connection mode for the Hermes Agent backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum HermesMode {
    /// Connect to an already-running Hermes gateway over HTTP/SSE.
    Api {
        /// Base URL of the Hermes Agent gateway (e.g. <http://127.0.0.1:8642>).
        api_base: String,
    },
    /// Spawn `hermes` CLI as a subprocess; communicate over stdio.
    Cli {
        /// Path to the `hermes` binary; defaults to "hermes" in PATH.
        bin: Option<String>,
    },
    /// Try API first; fall back to CLI if the gateway is unreachable.
    Auto {
        api_base: String,
        bin: Option<String>,
    },
}

impl Default for HermesMode {
    fn default() -> Self {
        HermesMode::Auto {
            api_base: DEFAULT_API_BASE.to_string(),
            bin: None,
        }
    }
}

/// Top-level configuration for the Hermes bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HermesBridgeConfig {
    #[serde(default)]
    pub mode: HermesMode,
    /// Directory for persistent state (thread index, runs, events).
    #[serde(default)]
    pub state_dir: Option<String>,
    /// Timeout for `/health` probes, milliseconds.
    #[serde(default = "default_health_timeout_ms")]
    pub health_timeout_ms: u64,
    /// TTL for cached availability decisions, milliseconds.
    #[serde(default = "default_health_cache_ttl_ms")]
    pub health_cache_ttl_ms: u64,
}

fn default_health_timeout_ms() -> u64 {
    1000
}

fn default_health_cache_ttl_ms() -> u64 {
    2000
}

impl Default for HermesBridgeConfig {
    fn default() -> Self {
        Self {
            mode: HermesMode::default(),
            state_dir: None,
            health_timeout_ms: default_health_timeout_ms(),
            health_cache_ttl_ms: default_health_cache_ttl_ms(),
        }
    }
}
