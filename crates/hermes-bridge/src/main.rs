//! Hermes bridge binary entry point.

use std::{path::PathBuf, sync::Arc};

#[cfg(unix)]
use alleycat_bridge_core::ServerOptions;
use alleycat_hermes_bridge::{HermesBridge, HermesBridgeConfig, HermesMode};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let bridge = Arc::new(HermesBridge::new(config_from_env()));

    match socket_arg() {
        Some(path) => {
            #[cfg(unix)]
            {
                alleycat_bridge_core::serve_unix(
                    bridge,
                    ServerOptions {
                        socket_path: path,
                        unlink_stale: true,
                    },
                )
                .await
            }
            #[cfg(not(unix))]
            {
                let _ = bridge;
                anyhow::bail!("Unix socket transport is not supported on this platform")
            }
        }
        None => alleycat_bridge_core::serve_stdio(bridge).await,
    }
}

fn config_from_env() -> HermesBridgeConfig {
    let mode = match std::env::var("HERMES_BRIDGE_MODE")
        .unwrap_or_else(|_| "auto".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "api" => HermesMode::Api {
            api_base: std::env::var("HERMES_API_BASE")
                .unwrap_or_else(|_| "http://127.0.0.1:8642".to_string()),
        },
        "cli" => HermesMode::Cli {
            bin: std::env::var("HERMES_BRIDGE_BIN").ok(),
        },
        _ => HermesMode::Auto {
            api_base: std::env::var("HERMES_API_BASE")
                .unwrap_or_else(|_| "http://127.0.0.1:8642".to_string()),
            bin: std::env::var("HERMES_BRIDGE_BIN").ok(),
        },
    };
    HermesBridgeConfig {
        mode,
        state_dir: std::env::var("HERMES_BRIDGE_STATE_DIR").ok(),
    }
}

fn socket_arg() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--socket" || arg == "--listen" {
            return args.next().map(PathBuf::from);
        }
    }
    std::env::var_os("ALLEYCAT_BRIDGE_SOCKET").map(PathBuf::from)
}
