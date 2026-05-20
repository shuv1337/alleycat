use clap::Args;

use crate::agent_manifest::MANIFESTS;
use crate::cli;
use crate::daemon::control::{Request, StatusInfo, token_fingerprint};
use crate::ipc;
use crate::paths;
use crate::protocol::AgentInfo;

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Emit machine-readable JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,

    /// Only consult the running daemon over IPC; do not fall back to a
    /// locally-rendered offline status. Returns a non-zero exit code if
    /// no daemon control socket is reachable. Use this from scripts and
    /// shell wrappers that just want a cheap "is the daemon up?" probe
    /// without paying for config load, manifest enumeration, or any
    /// agent-availability work.
    #[arg(long)]
    pub ipc_only: bool,
}

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let info = if ipc::is_daemon_running().await {
        let resp = cli::send(Request::Status).await?;
        cli::decode_data::<StatusInfo>(resp)?
    } else if args.ipc_only {
        anyhow::bail!(
            "alleycat daemon not running (no control socket reachable); \
             --ipc-only refuses the offline fallback"
        );
    } else {
        offline_status().await?
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }

    println!("{} daemon", crate::binary_name());
    println!("  pid:               {}", info.pid);
    println!(
        "  version:           {}",
        info.version.as_deref().unwrap_or("<unknown>")
    );
    println!("  node id:           {}", info.node_id);
    println!("  token (sha256/16): {}", info.token_short);
    println!(
        "  relay:             {}",
        info.relay.as_deref().unwrap_or("<iroh default>")
    );
    println!("  config:            {}", info.config_path);
    if info.uptime_secs > 0 {
        println!("  uptime (s):        {}", info.uptime_secs);
    } else {
        println!("  uptime (s):        <daemon not running>");
    }
    println!("  agents:");
    for agent in &info.agents {
        println!(
            "    {} display=\"{}\" wire={} available={}",
            agent.name,
            agent.display_name,
            agent.wire.as_str(),
            agent.available
        );
    }
    Ok(())
}

/// Status when the daemon isn't running. Pid is 0 and uptime is 0 so the
/// human renderer can call out the offline state.
///
/// Historically this constructed a full [`AgentManager`] just to compute
/// per-agent availability, which spun up the Hermes bridge, ACP pools, pi
/// RPC probes, eviction tasks, and other startup work — taking ~10 seconds
/// before printing anything. That was both surprising and dangerous: the
/// `codex` wrapper script calls `alleycat status` during alleycat's own
/// startup to decide whether to redirect to `app-server proxy`, and the
/// 10-second pause inside that wrapper raced with alleycat's 5-second
/// `codex app-server --help` probe timeout, producing the
/// `codex app-server --help timed out` warning on every restart.
///
/// The lightweight version below skips bridge construction entirely. It
/// loads config and the secret key (small file reads), enumerates agents
/// straight from [`MANIFESTS`] with `available: false` (the daemon is
/// offline, so nothing is actually serveable regardless of which
/// binaries happen to be on PATH), and returns. Typical runtime is
/// sub-millisecond.
async fn offline_status() -> anyhow::Result<StatusInfo> {
    let cfg = crate::config::load_or_init().await?;
    let secret_key = crate::state::load_or_create_secret_key().await?;
    let agent_list: Vec<AgentInfo> = MANIFESTS
        .iter()
        .map(|manifest| AgentInfo {
            name: manifest.name.to_owned(),
            display_name: manifest.display_name.to_owned(),
            wire: manifest.wire.clone(),
            available: false,
            presentation: Some(manifest.presentation()),
            capabilities: Some(manifest.capabilities()),
        })
        .collect();
    Ok(StatusInfo {
        pid: 0,
        node_id: secret_key.public().to_string(),
        token_short: token_fingerprint(&cfg.token),
        relay: cfg.relay.clone(),
        config_path: paths::host_config_file()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string()),
        uptime_secs: 0,
        agents: agent_list,
        version: Some(crate::binary_version().to_string()),
    })
}
