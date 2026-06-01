use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use alleycat_acp_bridge::AcpBridge;
use alleycat_amp_bridge::AmpBridge;
use alleycat_bridge_core::codex_resolver::{newest_codex_candidates_first, program_candidates};
use alleycat_bridge_core::session::{Session, SessionRegistry, SessionRegistryConfig};
use alleycat_bridge_core::{
    Bridge, Conn, InboundMessage, JsonRpcMessage, JsonRpcResponse, JsonRpcVersion,
    LaunchEnvironment, LaunchEnvironmentResolver, LocalLauncher, ProcessLauncher,
    UserEnvironmentLauncher,
};
use alleycat_claude_bridge::ClaudeBridge;
use alleycat_codex_remote_control::{
    CodexRemoteControlConfig, CodexRemoteControlHandle, CodexRemoteControlSnapshot,
    CodexRemoteControlTuning,
};
use alleycat_devin_bridge::DevinBridge;
use alleycat_droid_bridge::DroidBridge;
use alleycat_grok_bridge::GrokBridge;
use alleycat_hermes_bridge::{HermesBridge, HermesBridgeConfig};
use alleycat_opencode_bridge::OpencodeBridge;
use alleycat_pi_bridge::PiBridge;
use alleycat_shell_bridge::ShellBridge;
use anyhow::{Context, anyhow};
use arc_swap::ArcSwap;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, OnceCell, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::agent_manifest::{MANIFESTS, manifest_for};
use crate::config::HostConfig;
use crate::protocol::{AgentInfo, AgentWire};
use crate::stream::IrohStream;

/// Stable identifier for a JSON-RPC bridge agent. Codex is intentionally
/// excluded — Alleycat talks to it directly through Codex's app-server
/// transport, using the best mode supported by the local codex binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Pi,
    Amp,
    Claude,
    Opencode,
    Droid,
    Hermes,
    Devin,
    Grok,
    Shell,
}

/// How the daemon talks to `codex app-server`. Selected at startup by probing
/// the user-installed `codex` binary, then cached for the daemon's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexMode {
    /// Upstream lifecycle manager: `<bin> app-server daemon start`, then one
    /// `<bin> app-server proxy --sock <daemon socket>` child per iroh stream.
    /// This is Codex's intended remote/SSH app-server management path.
    UnixDaemon,
    /// `<bin> app-server --listen unix://` plus one `<bin> app-server proxy`
    /// child per iroh stream. Compatibility path for CLIs without the upstream
    /// daemon command.
    UnixProxy,
    /// `<bin> app-server --listen ws://host:port` — one shared child for
    /// the daemon lifetime, multi-client over websocket. Works on
    /// codex-cli versions that grew the `--listen` flag (≥ early 2026).
    Websocket,
    /// `<bin> app-server` — one fresh child per iroh stream, JSON-RPC
    /// over stdio. Works on every codex version that has the `app-server`
    /// subcommand.
    Stdio,
}

struct CodexDetection {
    mode: CodexMode,
    bin: PathBuf,
    available: bool,
}

#[derive(Clone)]
struct CodexUnixEndpoint {
    bin: PathBuf,
    /// `None` means Codex's default control socket. `Some` is an explicit socket
    /// passed to `app-server proxy --sock` and, for legacy child-owned startup,
    /// to `--listen unix://PATH`.
    socket_path: Option<PathBuf>,
}

impl CodexUnixEndpoint {
    fn default_socket(bin: PathBuf) -> Self {
        Self {
            bin,
            socket_path: None,
        }
    }

    fn custom_socket(bin: PathBuf, socket_path: PathBuf) -> Self {
        Self {
            bin,
            socket_path: Some(socket_path),
        }
    }

    fn listen_url(&self) -> String {
        match self.socket_path.as_deref() {
            Some(socket_path) => format!("unix://{}", socket_path.display()),
            None => "unix://".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct AgentManager {
    config: Arc<ArcSwap<HostConfig>>,
    bridges: HashMap<AgentKind, Arc<dyn Bridge>>,
    /// Opencode is built lazily because constructing it spawns the opencode
    /// child + opens an SSE subscription; we don't want to pay that cost on
    /// daemon startup if no client ever asks for opencode.
    opencode_bridge: Arc<OnceCell<Arc<OpencodeBridge>>>,
    /// One daemon-owned `codex app-server` child for modes that keep a shared
    /// app-server alive (`UnixProxy` or legacy `Websocket`). Not populated when
    /// Alleycat is proxying to an externally-started Codex app-server.
    codex_child: Arc<Mutex<Option<Child>>>,
    /// Native remote-control enablement loop for the Alleycat-owned Codex
    /// UnixProxy app-server. The crate owns protocol details; AgentManager only
    /// starts, stops, and reports its status.
    codex_remote_control: CodexRemoteControlHandle,
    /// Detected once at startup. Determines whether `serve_codex` runs the
    /// upstream daemon proxy, legacy Unix proxy, legacy websocket byte-pump, or
    /// per-stream stdio bridging.
    codex_mode: CodexMode,
    /// The resolved codex executable selected during startup probing.
    codex_bin: PathBuf,
    /// Whether the selected codex executable could be spawned.
    codex_available: bool,
    launch_env: LaunchEnvironmentResolver,
    session_registry: Arc<SessionRegistry>,
    /// Held to keep the registry's reaper alive for the daemon lifetime.
    _reaper_handle: Arc<tokio::task::JoinHandle<()>>,
}

impl AgentManager {
    pub async fn new(config: Arc<ArcSwap<HostConfig>>) -> anyhow::Result<Self> {
        let snapshot = config.load();

        let launch_env = LaunchEnvironmentResolver::default();
        let daemon_cwd = std::env::current_dir().ok();
        let daemon_env = launch_env.resolve(daemon_cwd.as_deref()).await;

        // Honor `CODEX_HOME` from the same resolved launch environment used
        // for child processes, so bridge indexes/config and spawned agents all
        // agree even when launchd/systemd did not inherit the user's shell env.
        let codex_home = env_path(&daemon_env, "CODEX_HOME");
        if let Some(ref home) = codex_home {
            tokio::fs::create_dir_all(home)
                .await
                .with_context(|| format!("creating {}", home.display()))?;
        }

        let base_launcher: Arc<dyn ProcessLauncher> = Arc::new(LocalLauncher);
        let user_launcher =
            UserEnvironmentLauncher::with_resolver(base_launcher, launch_env.clone())
                .with_program_aliases(pi_program_aliases());
        let launcher: Arc<dyn ProcessLauncher> = Arc::new(user_launcher);

        let mut pi_builder = PiBridge::builder()
            .agent_bin(PathBuf::from(&snapshot.agents.pi.bin))
            .launcher(Arc::clone(&launcher));
        if let Some(ref home) = codex_home {
            pi_builder = pi_builder.codex_home(home.clone());
        }
        let pi_bridge = pi_builder.build().await.context("building pi bridge")?;

        let mut amp_builder = AmpBridge::builder()
            .agent_bin(PathBuf::from(&snapshot.agents.amp.bin))
            .launcher(Arc::clone(&launcher))
            .dangerously_allow_all(snapshot.agents.amp.dangerously_allow_all);
        if let Some(ref home) = codex_home {
            amp_builder = amp_builder.codex_home(home.clone());
        }
        let amp_bridge = amp_builder.build().await.context("building amp bridge")?;

        let mut claude_builder = ClaudeBridge::builder()
            .agent_bin(PathBuf::from(&snapshot.agents.claude.bin))
            .launcher(Arc::clone(&launcher))
            .bypass_permissions(snapshot.agents.claude.bypass_permissions);
        if let Some(ref home) = codex_home {
            claude_builder = claude_builder.codex_home(home.clone());
        }
        let claude_bridge = claude_builder
            .build()
            .await
            .context("building claude bridge")?;

        let mut droid_builder = DroidBridge::builder()
            .agent_bin(PathBuf::from(&snapshot.agents.droid.bin))
            .launcher(Arc::clone(&launcher));
        if let Some(ref home) = codex_home {
            droid_builder = droid_builder.codex_home(home.clone());
        }
        let droid_bridge = droid_builder
            .build()
            .await
            .context("building droid bridge")?;

        let devin_builder = AcpBridge::builder()
            .agent_bin(PathBuf::from(&snapshot.agents.devin.bin))
            .launcher(Arc::clone(&launcher));
        let devin_acp = devin_builder
            .build()
            .await
            .context("building devin bridge")?;
        // Wrap the generic ACP bridge so `thread/list` reads devin's local
        // SQLite store directly; ACP `session/list` filters out
        // untitled/low-activity sessions and the mobile UI wants everything.
        let devin_bridge: Arc<dyn Bridge> =
            Arc::new(DevinBridge::with_default_db(devin_acp).context("wiring devin bridge")?);

        // Grok is another ACP agent; launch via `grok agent stdio`
        // (note: unlike Devin we do not assume a sessions.db for thread/list).
        // All Grok launch knowledge lives in `grok-bridge`.
        // The daemon and acp-bridge stay unaware of "agent", "stdio", etc.
        let grok_bridge = GrokBridge::build(
            PathBuf::from(&snapshot.agents.grok.bin),
            snapshot.agents.grok.no_leader,
            snapshot.agents.grok.model.clone(),
            snapshot.agents.grok.always_approve,
            snapshot.agents.grok.reasoning_effort.clone(),
            Arc::clone(&launcher),
        )
        .await
        .context("building grok bridge")?;

        let shell_cfg = &snapshot.agents.shell;
        let mut shell_builder = ShellBridge::builder()
            .shell_bin(shell_cfg.shell_bin.clone())
            .allow_env_passthrough(shell_cfg.allow_env_passthrough);
        if let Some(default_cwd) = shell_cfg.default_cwd.as_ref() {
            shell_builder = shell_builder.default_cwd(default_cwd);
        }
        let shell_bridge = shell_builder.build();

        let mut bridges: HashMap<AgentKind, Arc<dyn Bridge>> = HashMap::new();
        bridges.insert(AgentKind::Pi, pi_bridge as Arc<dyn Bridge>);
        bridges.insert(AgentKind::Amp, amp_bridge as Arc<dyn Bridge>);
        bridges.insert(AgentKind::Claude, claude_bridge as Arc<dyn Bridge>);
        bridges.insert(AgentKind::Droid, droid_bridge as Arc<dyn Bridge>);
        bridges.insert(AgentKind::Devin, devin_bridge);
        bridges.insert(AgentKind::Grok, grok_bridge);
        bridges.insert(AgentKind::Shell, shell_bridge);

        let hermes_cfg = &snapshot.agents.hermes;
        let hermes_bridge_cfg = HermesBridgeConfig {
            mode: alleycat_hermes_bridge::HermesMode::Auto {
                api_base: hermes_cfg.api_base.clone(),
                bin: Some(hermes_cfg.bin.clone()),
            },
            state_dir: codex_home
                .as_ref()
                .map(|p| p.join("hermes-bridge").to_string_lossy().to_string()),
        };
        bridges.insert(
            AgentKind::Hermes,
            Arc::new(HermesBridge::new(hermes_bridge_cfg)) as Arc<dyn Bridge>,
        );

        let session_cfg = &snapshot.session;
        let registry_config = SessionRegistryConfig {
            ring_max_msgs: session_cfg.replay_max_msgs,
            ring_max_bytes: session_cfg.replay_max_bytes,
            idle_ttl: std::time::Duration::from_secs(session_cfg.idle_ttl_secs),
            pending_grace: std::time::Duration::from_secs(session_cfg.pending_grace_secs),
        };
        let session_registry = SessionRegistry::new(registry_config);
        let reaper_handle = Arc::new(session_registry.spawn_reaper());

        let codex_detection = if snapshot.agents.codex.enabled {
            detect_codex(&snapshot.agents.codex.bin, &daemon_env).await
        } else {
            // Doesn't matter; codex is disabled. Pick a default so the
            // field has a value.
            CodexDetection {
                mode: CodexMode::Stdio,
                bin: PathBuf::from(&snapshot.agents.codex.bin),
                available: false,
            }
        };
        info!(
            codex_mode = ?codex_detection.mode,
            configured_bin = %snapshot.agents.codex.bin,
            bin = %codex_detection.bin.display(),
            available = codex_detection.available,
            "codex transport mode"
        );

        Ok(Self {
            config,
            bridges,
            opencode_bridge: Arc::new(OnceCell::new()),
            codex_child: Arc::new(Mutex::new(None)),
            codex_remote_control: CodexRemoteControlHandle::new(),
            codex_mode: codex_detection.mode,
            codex_bin: codex_detection.bin,
            codex_available: codex_detection.available,
            launch_env,
            session_registry,
            _reaper_handle: reaper_handle,
        })
    }

    pub fn session_registry(&self) -> &Arc<SessionRegistry> {
        &self.session_registry
    }

    /// Eagerly start the single shared opencode server at daemon boot when
    /// `[agents.opencode] enabled = true` and `discoverable = true`. This makes
    /// the managed `opencode serve --discoverable` available immediately so a
    /// bare local `opencode` TUI (launched before any paired alleycat opencode
    /// session) attaches to it via `~/.local/state/opencode/server.json`
    /// instead of spawning a competing server.
    ///
    /// Non-fatal: a missing or incompatible opencode binary logs a warning and
    /// leaves the bridge uninitialized; the next paired opencode session retries
    /// lazily through `opencode_bridge_arc`.
    pub async fn autostart_opencode_if_discoverable(&self) {
        let (enabled, discoverable) = {
            let cfg = self.config.load();
            (
                cfg.agents.opencode.enabled,
                cfg.agents.opencode.discoverable,
            )
        };
        if !enabled || !discoverable {
            return;
        }
        match self.opencode_bridge_arc().await {
            Ok(_) => info!("opencode discoverable server autostarted"),
            Err(error) => warn!("opencode discoverable autostart skipped: {error:#}"),
        }
    }

    /// Local loopback entrypoint for desktop clients that speak Codex's
    /// websocket app-server wire directly. For modern Codex builds Alleycat
    /// owns a Unix app-server child and bridges each accepted TCP client through
    /// `codex app-server proxy`; for older websocket-only builds it starts the
    /// shared Codex websocket process on the configured host/port.
    pub async fn serve_codex_local_tcp_bridge(&self, shutdown: Arc<Notify>) -> anyhow::Result<()> {
        let (enabled, host, port) = {
            let cfg = self.config.load();
            (
                cfg.agents.codex.enabled,
                cfg.agents.codex.host.clone(),
                cfg.agents.codex.port,
            )
        };
        if !enabled {
            return Ok(());
        }

        match self.codex_mode {
            CodexMode::Stdio => {
                warn!(
                    %host,
                    port,
                    "codex local websocket bridge unavailable in stdio mode"
                );
                shutdown.notified().await;
                return Ok(());
            }
            CodexMode::Websocket => {
                let (host, port) = self.ensure_codex_running().await?;
                info!(
                    %host,
                    port,
                    "codex local websocket listener is managed by Codex child"
                );
                shutdown.notified().await;
                return Ok(());
            }
            CodexMode::UnixDaemon | CodexMode::UnixProxy => {}
        }

        let listener = TcpListener::bind((host.as_str(), port))
            .await
            .with_context(|| format!("binding codex local websocket bridge on {host}:{port}"))?;
        info!(
            %host,
            port,
            "codex local websocket bridge listening"
        );

        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    info!("codex local websocket bridge shutting down");
                    return Ok(());
                }
                accepted = listener.accept() => {
                    let (stream, peer_addr) = accepted.context("accepting codex local websocket client")?;
                    let manager = self.clone();
                    tokio::spawn(async move {
                        if let Err(error) = manager.serve_codex(stream).await {
                            warn!(%peer_addr, "codex local websocket client ended with error: {error:#}");
                        }
                    });
                }
            }
        }
    }

    /// Local websocket JSON-RPC router for Codex Desktop provider experiments.
    ///
    /// The production Codex bridge remains on the configured Codex port
    /// (`8390` by default). This router listens on `port + 1` and serves one
    /// non-Codex Alleycat bridge selected by the request path:
    ///
    /// - `ws://127.0.0.1:8391/agent/claude`
    /// - `ws://127.0.0.1:8391/agent/opencode`
    ///
    /// Keeping this on a separate port lets the default Desktop launch continue
    /// to byte-proxy directly to Codex while we validate provider-backed
    /// Desktop sessions.
    pub async fn serve_desktop_agent_router(&self, shutdown: Arc<Notify>) -> anyhow::Result<()> {
        let (host, port) = {
            let cfg = self.config.load();
            let port =
                cfg.agents.codex.port.checked_add(1).ok_or_else(|| {
                    anyhow!("codex port cannot be incremented for desktop router")
                })?;
            (cfg.agents.codex.host.clone(), port)
        };
        let listener = TcpListener::bind((host.as_str(), port))
            .await
            .with_context(|| format!("binding desktop agent router on {host}:{port}"))?;
        info!(%host, port, "desktop agent router listening");

        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    info!("desktop agent router shutting down");
                    return Ok(());
                }
                accepted = listener.accept() => {
                    let (stream, peer_addr) = accepted.context("accepting desktop agent router client")?;
                    let manager = self.clone();
                    tokio::spawn(async move {
                        if let Err(error) = manager.serve_desktop_agent_router_client(stream).await {
                            warn!(%peer_addr, "desktop agent router client ended with error: {error:#}");
                        }
                    });
                }
            }
        }
    }

    async fn serve_desktop_agent_router_client(&self, stream: TcpStream) -> anyhow::Result<()> {
        let selected_agent = Arc::new(StdMutex::new(None::<String>));
        let selected_for_handshake = Arc::clone(&selected_agent);
        let ws = tokio_tungstenite::accept_hdr_async(
            stream,
            move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                  response| {
                let agent = desktop_router_agent_from_uri(request.uri());
                *selected_for_handshake
                    .lock()
                    .expect("selected agent mutex poisoned") = agent;
                Ok(response)
            },
        )
        .await
        .context("accepting desktop router websocket")?;
        let agent = selected_agent
            .lock()
            .expect("selected agent mutex poisoned")
            .clone()
            .ok_or_else(|| anyhow!("desktop router websocket path must include /agent/<name>"))?;
        let kind = agent_kind_from_str(&agent)
            .ok_or_else(|| anyhow!("unknown desktop agent `{agent}`"))?;
        if !self.config.load().agents.is_enabled(kind) {
            return Err(anyhow!("agent `{agent}` is disabled"));
        }
        let bridge = self.bridge_for_kind(kind).await?;
        let agent_id = agent_kind_str(kind);
        let session = self
            .session_registry
            .get_or_create(format!("desktop-local-{agent_id}"), agent_id);
        let conn = Conn::from_session(Arc::clone(&session));
        let (mut ws_write, mut ws_read) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();

        let writer_task = tokio::spawn(async move {
            while let Some(value) = out_rx.recv().await {
                let text = match serde_json::to_string(&value) {
                    Ok(text) => text,
                    Err(error) => {
                        warn!("failed serializing desktop router frame: {error:#}");
                        continue;
                    }
                };
                if ws_write.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        });

        let drainer_session = Arc::clone(&session);
        let drainer_tx = out_tx.clone();
        let drainer_task = tokio::spawn(async move {
            drain_desktop_router_session(drainer_session, drainer_tx).await;
        });

        info!(agent = agent_id, "desktop agent router client attached");
        while let Some(message) = ws_read.next().await {
            let message = message.context("reading desktop router websocket message")?;
            let value = match message {
                Message::Text(text) => serde_json::from_str::<Value>(&text)
                    .context("parsing desktop router JSON text frame")?,
                Message::Binary(bytes) => serde_json::from_slice::<Value>(&bytes)
                    .context("parsing desktop router JSON binary frame")?,
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            let inbound = match InboundMessage::from_value(value.clone()) {
                Ok(inbound) => inbound,
                Err(error) => {
                    warn!(raw = %value, "discarding malformed desktop router JSON-RPC frame: {error}");
                    continue;
                }
            };
            match inbound {
                InboundMessage::Request(request) => {
                    let bridge = Arc::clone(&bridge);
                    let conn = conn.clone();
                    let out_tx = out_tx.clone();
                    tokio::spawn(async move {
                        let id = request.id;
                        let method = request.method;
                        let params = request.params.unwrap_or(Value::Null);
                        let result = if method == "initialize" {
                            conn.set_initialize_capabilities(&params);
                            bridge.initialize(&conn, params).await
                        } else if let Some(result) = desktop_router_stub_response(&method) {
                            Ok(result)
                        } else {
                            bridge.dispatch(&conn, &method, params).await
                        };
                        let response = match result {
                            Ok(result) => JsonRpcResponse {
                                jsonrpc: JsonRpcVersion,
                                id,
                                result: Some(result),
                                error: None,
                            },
                            Err(error) => JsonRpcResponse {
                                jsonrpc: JsonRpcVersion,
                                id,
                                result: None,
                                error: Some(error),
                            },
                        };
                        let value = serde_json::to_value(JsonRpcMessage::Response(response))
                            .unwrap_or_else(|_| {
                                json!({
                                    "jsonrpc": "2.0",
                                    "error": {
                                        "code": -32603,
                                        "message": "failed to encode response"
                                    }
                                })
                            });
                        let _ = out_tx.send(value);
                    });
                }
                InboundMessage::Notification(notification) => {
                    bridge
                        .notification(
                            &conn,
                            &notification.method,
                            notification.params.unwrap_or(Value::Null),
                        )
                        .await;
                }
                InboundMessage::Response(response) => {
                    conn.notifier().resolve_response(response).await;
                }
            }
        }

        session.drop_attachment();
        drop(out_tx);
        let _ = drainer_task.await;
        let _ = writer_task.await;
        info!(agent = agent_id, "desktop agent router client detached");
        Ok(())
    }

    /// Fan out a shutdown call to every registered bridge. Called from
    /// the daemon's graceful shutdown path so each bridge can kill its
    /// child processes (ACP agents, claude, etc.) before the daemon
    /// returns. Without this, the tokio runtime Drop chain is the only
    /// thing keeping `kill_on_drop` honest — and that's not reliable
    /// on process exit, which is how we ended up with multiple
    /// `devin acp` / `grok agent stdio` zombies between restarts.
    pub async fn shutdown(&self) {
        for (kind, bridge) in &self.bridges {
            info!(agent = agent_kind_str(*kind), "shutting down bridge");
            bridge.shutdown().await;
        }
        if let Some(opencode) = self.opencode_bridge.get() {
            opencode.shutdown().await;
        }
        self.codex_remote_control.stop().await;
        self.stop_codex_child().await;
    }

    pub async fn codex_remote_control_status(&self) -> Option<CodexRemoteControlSnapshot> {
        if self.codex_mode == CodexMode::UnixProxy {
            Some(self.codex_remote_control.status().await)
        } else {
            None
        }
    }

    pub async fn list_agents(&self) -> Vec<AgentInfo> {
        // Availability is computed per-agent (some are async, some not),
        // then each manifest is rendered to the wire `AgentInfo` shape.
        let launch_env = self.daemon_launch_env().await;
        let mut out = Vec::with_capacity(MANIFESTS.len());
        for manifest in MANIFESTS {
            let available = match manifest.name {
                "codex" => self.codex_available(),
                "pi" => self.pi_available(&launch_env),
                "amp" => self.amp_available(&launch_env),
                "opencode" => self.opencode_available(&launch_env),
                "claude" => self.claude_available(&launch_env),
                "droid" => self.droid_available(&launch_env),
                "hermes" => self.hermes_available(&launch_env).await,
                "devin" => self.devin_available(&launch_env),
                "grok" => self.grok_available(&launch_env),
                "shell" => self.shell_available(),
                _ => false,
            };
            let wire = if manifest.name == "codex" {
                match self.codex_mode {
                    CodexMode::UnixDaemon => AgentWire::Websocket,
                    CodexMode::UnixProxy => AgentWire::Websocket,
                    CodexMode::Websocket => AgentWire::Websocket,
                    CodexMode::Stdio => AgentWire::Jsonl,
                }
            } else {
                manifest.wire.clone()
            };
            out.push(AgentInfo {
                name: manifest.name.to_owned(),
                display_name: manifest.display_name.to_owned(),
                wire,
                available,
                presentation: Some(manifest.presentation()),
                capabilities: Some(manifest.capabilities()),
            });
        }
        out
    }

    /// Static manifest lookup for telemetry / debugging. Returns the
    /// stable manifest for a known agent name, not the live availability
    /// state.
    #[allow(dead_code)]
    pub fn manifest_for(name: &str) -> Option<&'static crate::agent_manifest::AgentManifest> {
        manifest_for(name)
    }

    /// Session-aware dispatch: the iroh stream attaches to the supplied
    /// session and survives a client disconnect.
    pub async fn serve_agent_with_session(
        &self,
        agent: &str,
        stream: IrohStream,
        session: Arc<Session>,
        last_seen: Option<u64>,
    ) -> anyhow::Result<()> {
        match agent {
            // Codex doesn't participate in the JSON-RPC replay scheme —
            // each iroh stream is a fresh websocket client to the shared
            // codex app-server, and codex has its own resume semantics
            // (SQLite session store). The session is held just so the
            // registry's accounting stays uniform; its ring stays empty.
            "codex" => {
                let _ = (session, last_seen);
                self.serve_codex(stream).await
            }
            other => {
                let kind =
                    agent_kind_from_str(other).ok_or_else(|| anyhow!("unknown agent `{other}`"))?;
                self.serve_with_session(kind, stream, session, last_seen)
                    .await
            }
        }
    }

    /// Session-aware dispatch for browser WebSocket streams. Codex currently
    /// stays on the native iroh stream path because its app-server proxy code
    /// depends on iroh's bidirectional stream wrapper.
    pub async fn serve_bridge_agent_with_session<S>(
        &self,
        agent: &str,
        stream: S,
        session: Arc<Session>,
        last_seen: Option<u64>,
    ) -> anyhow::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let kind = agent_kind_from_str(agent)
            .ok_or_else(|| anyhow!("agent `{agent}` is not available over websocket"))?;
        self.serve_with_session(kind, stream, session, last_seen)
            .await
    }

    /// Polymorphic Bridge dispatch. Pi/Claude come straight from the eagerly-
    /// built `bridges` map; opencode initializes lazily on first use.
    pub async fn serve_with_session<S>(
        &self,
        kind: AgentKind,
        stream: S,
        session: Arc<Session>,
        last_seen: Option<u64>,
    ) -> anyhow::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        if !self.config.load().agents.is_enabled(kind) {
            return Err(anyhow!("agent `{}` is disabled", agent_kind_str(kind)));
        }
        let bridge = self.bridge_for_kind(kind).await?;
        alleycat_bridge_core::serve_stream_with_session(bridge, stream, session, last_seen)
            .await
            .with_context(|| format!("serving `{}` bridge stream", agent_kind_str(kind)))
    }

    async fn bridge_for_kind(&self, kind: AgentKind) -> anyhow::Result<Arc<dyn Bridge>> {
        match kind {
            AgentKind::Opencode => {
                let oc = self.opencode_bridge_arc().await?;
                Ok(oc as Arc<dyn Bridge>)
            }
            other => self
                .bridges
                .get(&other)
                .cloned()
                .ok_or_else(|| anyhow!("agent `{}` is not configured", agent_kind_str(other))),
        }
    }

    /// Stable static name for a wire-supplied agent string, used to key the
    /// session registry. Returns `None` for unknown agents.
    pub fn agent_id(name: &str) -> Option<&'static str> {
        match name {
            "codex" => Some("codex"),
            "pi" => Some("pi"),
            "amp" => Some("amp"),
            "opencode" => Some("opencode"),
            "claude" => Some("claude"),
            "droid" => Some("droid"),
            "hermes" => Some("hermes"),
            "devin" => Some("devin"),
            "grok" => Some("grok"),
            "shell" => Some("shell"),
            _ => None,
        }
    }

    pub fn agent_enabled(&self, agent: &str) -> bool {
        let cfg = self.config.load();
        match agent {
            "codex" => cfg.agents.codex.enabled,
            "pi" => cfg.agents.pi.enabled,
            "amp" => cfg.agents.amp.enabled,
            "opencode" => cfg.agents.opencode.enabled,
            "claude" => cfg.agents.claude.enabled,
            "droid" => cfg.agents.droid.enabled,
            "hermes" => cfg.agents.hermes.enabled,
            "devin" => cfg.agents.devin.enabled,
            "grok" => cfg.agents.grok.enabled,
            "shell" => cfg.agents.shell.enabled,
            _ => false,
        }
    }

    pub async fn restart_agent(&self, agent: &str) -> anyhow::Result<()> {
        match agent {
            "codex" => self.restart_codex().await,
            other => Err(anyhow!("restart is not supported for agent `{}`", other)),
        }
    }

    async fn restart_codex(&self) -> anyhow::Result<()> {
        match self.codex_mode {
            CodexMode::UnixDaemon => {
                // Defense-in-depth: detect_codex_runtime() never returns
                // UnixDaemon on this fork, but keep the branch wired up so
                // operators who override the mode explicitly still get a
                // working restart path.
                let bin = self.codex_bin.clone();
                let env = self.daemon_launch_env().await;
                run_codex_app_server_daemon(&bin, "restart", &env)
                    .await
                    .map(|_| ())?;
            }
            CodexMode::UnixProxy => {
                self.codex_remote_control.stop().await;
                self.stop_codex_child().await;
                let endpoint = self.ensure_codex_unix_running().await?;
                info!(
                    socket = ?endpoint.socket_path,
                    "codex app-server UnixProxy restarted"
                );
            }
            CodexMode::Websocket => {
                self.stop_codex_child().await;
                let (host, port) = self.ensure_codex_running().await?;
                info!(%host, port, "codex websocket app-server restarted");
            }
            CodexMode::Stdio => {
                // Stdio mode has no shared app-server to restart; each client
                // stream owns its own child. Treat restart as a successful
                // no-op so mobile Home remains safe on older Codex builds.
                info!("codex stdio mode has no shared app-server to restart");
            }
        }
        Ok(())
    }

    async fn stop_codex_child(&self) -> bool {
        self.codex_remote_control.stop().await;
        let mut guard = self.codex_child.lock().await;
        if let Some(mut child) = guard.take() {
            terminate_codex_child(&mut child, "app-server").await;
            info!("codex app-server child stopped");
            return true;
        }
        false
    }

    async fn serve_codex<S>(&self, stream: S) -> anyhow::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        match self.codex_mode {
            CodexMode::UnixDaemon => self.serve_codex_unix_proxy(stream).await,
            CodexMode::UnixProxy => self.serve_codex_unix_proxy(stream).await,
            CodexMode::Websocket => self.serve_codex_ws(stream).await,
            CodexMode::Stdio => self.serve_codex_stdio(stream).await,
        }
    }

    async fn serve_codex_unix_proxy<S>(&self, client_stream: S) -> anyhow::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let endpoint = if self.codex_mode == CodexMode::UnixDaemon {
            self.ensure_codex_daemon_running().await?
        } else {
            self.ensure_codex_unix_running().await?
        };
        let env = self.daemon_launch_env().await;
        let mut command = codex_command(&endpoint.bin);
        apply_launch_env_to_command(&mut command, &env);
        command.arg("app-server").arg("proxy");
        if let Some(socket_path) = endpoint.socket_path.as_deref() {
            command.arg("--sock").arg(socket_path);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                let socket = endpoint
                    .socket_path
                    .as_deref()
                    .map(|path| format!(" --sock {}", path.display()))
                    .unwrap_or_default();
                format!(
                    "spawning `{} app-server proxy{socket}`",
                    endpoint.bin.display()
                )
            })?;
        let proxy_pid = child.id();
        info!(pid = ?proxy_pid, "codex app-server proxy spawned");

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                warn!(target: "codex", "{line}");
            }
        });

        let mut client_stream = client_stream;
        let mut child_io = tokio::io::join(stdout, stdin);
        let _ = tokio::io::copy_bidirectional(&mut client_stream, &mut child_io).await;
        drop(child_io);
        info!(pid = ?proxy_pid, "codex app-server proxy stream closed, reaping");
        reap_codex_stream_child(child, "app-server proxy").await;
        Ok(())
    }

    async fn serve_codex_ws<S>(&self, mut client_stream: S) -> anyhow::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (host, port) = self.ensure_codex_running().await?;
        let mut tcp = TcpStream::connect((host.as_str(), port))
            .await
            .with_context(|| format!("connecting to codex app-server at {host}:{port}"))?;
        let _ = tokio::io::copy_bidirectional(&mut client_stream, &mut tcp).await;
        Ok(())
    }

    /// Starts the upstream Codex app-server daemon idempotently, then uses the
    /// socket path reported by the daemon's JSON lifecycle output.
    async fn ensure_codex_daemon_running(&self) -> anyhow::Result<CodexUnixEndpoint> {
        let bin = {
            let cfg = self.config.load();
            if !cfg.agents.codex.enabled {
                return Err(anyhow!("codex agent is disabled"));
            }
            self.codex_bin.clone()
        };

        let env = self.daemon_launch_env().await;
        let output = run_codex_app_server_daemon(&bin, "start", &env).await?;
        let endpoint = match output.socket_path {
            Some(socket_path) => CodexUnixEndpoint::custom_socket(bin, socket_path),
            None => CodexUnixEndpoint::default_socket(bin),
        };
        probe_codex_app_server_proxy(&endpoint.bin, endpoint.socket_path.as_deref(), &env)
            .await
            .with_context(|| {
                let socket = endpoint
                    .socket_path
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "default".to_string());
                format!("codex app-server proxy could not reach daemon socket {socket}")
            })?;
        Ok(endpoint)
    }

    /// Ensures Codex's default Unix-socket app-server is reachable. If an
    /// external Codex daemon/Desktop already owns the socket, Alleycat leaves it
    /// alone and only starts per-stream `app-server proxy` children.
    async fn ensure_codex_unix_running(&self) -> anyhow::Result<CodexUnixEndpoint> {
        let bin = {
            let cfg = self.config.load();
            if !cfg.agents.codex.enabled {
                return Err(anyhow!("codex agent is disabled"));
            }
            self.codex_bin.clone()
        };

        let env = self.daemon_launch_env().await;
        let default_endpoint = match probe_codex_app_server_proxy(&bin, None, &env).await {
            Ok(()) => Some(CodexUnixEndpoint::default_socket(bin.clone())),
            Err(error) => {
                if let Some(socket_path) =
                    default_codex_control_socket_accepts_connections(&env).await
                {
                    warn!(
                        "codex default control socket accepts connections but proxy websocket handshake failed; using alleycat-owned socket default_socket={} error={error:#}",
                        socket_path.display()
                    );
                }
                None
            }
        };

        let mut guard = self.codex_child.lock().await;
        let endpoint = if let Some(default_endpoint) = default_endpoint {
            if let Some(child) = guard.as_mut() {
                match child.try_wait() {
                    Ok(None) => {
                        let endpoint = default_endpoint;
                        drop(guard);
                        self.start_codex_remote_control(&endpoint, &env).await;
                        return Ok(endpoint);
                    }
                    Ok(Some(status)) => {
                        info!("discarding exited codex app-server child status={status}");
                        *guard = None;
                    }
                    Err(error) => {
                        warn!("discarding codex app-server child after try_wait failed: {error}");
                        *guard = None;
                    }
                }
            }

            // A reachable default socket with no tracked child is likely an
            // external Codex daemon/Desktop (or an orphan from a prior build).
            // Mobile restart must remain actionable, so Alleycat uses its own
            // control socket instead of attaching to an app-server it cannot
            // restart safely.
            if let Some(socket_path) = default_codex_control_socket_path(&env) {
                warn!(
                    default_socket = %socket_path.display(),
                    "codex default control socket is not tracked by this daemon; using alleycat-owned socket"
                );
                CodexUnixEndpoint::custom_socket(
                    default_endpoint.bin,
                    alleycat_codex_control_socket_path(&socket_path),
                )
            } else {
                default_endpoint
            }
        } else if let Some(socket_path) =
            default_codex_control_socket_accepts_connections(&env).await
        {
            CodexUnixEndpoint::custom_socket(bin, alleycat_codex_control_socket_path(&socket_path))
        } else {
            CodexUnixEndpoint::default_socket(bin)
        };
        match probe_codex_app_server_proxy(&endpoint.bin, endpoint.socket_path.as_deref(), &env)
            .await
        {
            Ok(()) => {
                drop(guard);
                self.start_codex_remote_control(&endpoint, &env).await;
                return Ok(endpoint);
            }
            Err(error) => {
                if endpoint.socket_path.is_none()
                    && let Some(socket_path) =
                        default_codex_control_socket_accepts_connections(&env).await
                {
                    warn!(
                        "codex default control socket accepts connections but proxy websocket handshake failed; using alleycat-owned socket default_socket={} error={error:#}",
                        socket_path.display()
                    );
                    drop(guard);
                    return self
                        .ensure_codex_unix_running_with_endpoint(
                            CodexUnixEndpoint::custom_socket(
                                endpoint.bin,
                                alleycat_codex_control_socket_path(&socket_path),
                            ),
                            &env,
                        )
                        .await;
                }
            }
        }

        let endpoint = self
            .ensure_codex_unix_running_locked(endpoint, &env, &mut *guard)
            .await?;
        drop(guard);
        self.start_codex_remote_control(&endpoint, &env).await;
        Ok(endpoint)
    }

    async fn ensure_codex_unix_running_with_endpoint(
        &self,
        endpoint: CodexUnixEndpoint,
        env: &LaunchEnvironment,
    ) -> anyhow::Result<CodexUnixEndpoint> {
        if probe_codex_app_server_proxy(&endpoint.bin, endpoint.socket_path.as_deref(), env)
            .await
            .is_ok()
        {
            self.start_codex_remote_control(&endpoint, env).await;
            return Ok(endpoint);
        }
        let mut guard = self.codex_child.lock().await;
        let endpoint = self
            .ensure_codex_unix_running_locked(endpoint, env, &mut *guard)
            .await?;
        drop(guard);
        self.start_codex_remote_control(&endpoint, env).await;
        Ok(endpoint)
    }

    async fn ensure_codex_unix_running_locked(
        &self,
        endpoint: CodexUnixEndpoint,
        env: &LaunchEnvironment,
        guard: &mut Option<Child>,
    ) -> anyhow::Result<CodexUnixEndpoint> {
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(None) => {
                    if let Some(mut child) = guard.take() {
                        terminate_codex_child(&mut child, "app-server").await;
                        info!("restarting codex app-server child after failed proxy probe");
                    }
                }
                Ok(Some(status)) => {
                    info!("discarding exited codex app-server child status={status}");
                    *guard = None;
                }
                Err(error) => {
                    warn!("discarding codex app-server child after try_wait failed: {error}");
                    *guard = None;
                }
            }
        }

        if guard.is_none() {
            let listen_url = endpoint.listen_url();
            let mut command = codex_command(&endpoint.bin);
            apply_launch_env_to_command(&mut command, env);
            let mut child = command
                .arg("app-server")
                .arg("--listen")
                .arg(&listen_url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| {
                    format!(
                        "spawning `{} app-server --listen {listen_url}`",
                        endpoint.bin.display()
                    )
                })?;

            if let Some(stderr) = child.stderr.take() {
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        warn!(target: "codex", "{line}");
                    }
                });
            }

            *guard = Some(child);
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if probe_codex_app_server_proxy(&endpoint.bin, endpoint.socket_path.as_deref(), env)
                .await
                .is_ok()
            {
                return Ok(endpoint);
            }
            if let Some(child) = guard.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                *guard = None;
                return Err(anyhow!(
                    "codex app-server exited before app-server proxy became reachable: {status}"
                ));
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "codex app-server did not become reachable through app-server proxy within 5s"
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn start_codex_remote_control(
        &self,
        endpoint: &CodexUnixEndpoint,
        env: &LaunchEnvironment,
    ) {
        if self.codex_mode != CodexMode::UnixProxy {
            return;
        }
        let Some(socket_path) = codex_remote_control_socket_path(endpoint, env) else {
            warn!("codex remote control cannot resolve control socket path");
            return;
        };
        let Some(auth_path) = codex_auth_path(env) else {
            warn!("codex remote control cannot resolve auth path");
            return;
        };
        self.codex_remote_control
            .start_or_update(CodexRemoteControlConfig {
                socket_path,
                auth_path,
                tuning: CodexRemoteControlTuning::default(),
            })
            .await;
    }

    /// Per-stream stdio bridge for codex versions that don't support
    /// `--listen`. Each iroh stream gets its own `codex app-server` child;
    /// codex's on-disk session store handles resume across reconnects.
    async fn serve_codex_stdio<S>(&self, mut client_stream: S) -> anyhow::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let bin = {
            let cfg = self.config.load();
            if !cfg.agents.codex.enabled {
                return Err(anyhow!("codex agent is disabled"));
            }
            self.codex_bin.clone()
        };

        let env = self.daemon_launch_env().await;
        let mut command = codex_command(&bin);
        apply_launch_env_to_command(&mut command, &env);
        let mut child = command
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning `{} app-server`", bin.display()))?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                warn!(target: "codex", "{line}");
            }
        });

        let mut child_io = tokio::io::join(stdout, stdin);
        let _ = tokio::io::copy_bidirectional(&mut client_stream, &mut child_io).await;
        drop(child_io);
        reap_codex_stream_child(child, "app-server stdio").await;
        Ok(())
    }

    /// Ensures *something* is listening on the configured codex websocket
    /// address. If an externally-managed codex (or a previously-spawned
    /// child) is already accepting connections, we use it as-is and skip
    /// spawning. Otherwise we spawn `<bin> app-server --listen ws://...`
    /// and wait for the port to bind. Returns `(host, port)` for the
    /// byte-pump to dial.
    async fn ensure_codex_running(&self) -> anyhow::Result<(String, u16)> {
        let (bin, host, port) = {
            let cfg = self.config.load();
            if !cfg.agents.codex.enabled {
                return Err(anyhow!("codex agent is disabled"));
            }
            (
                self.codex_bin.clone(),
                cfg.agents.codex.host.clone(),
                cfg.agents.codex.port,
            )
        };

        // Fast path: port is already accepting connections.
        if TcpStream::connect((host.as_str(), port)).await.is_ok() {
            return Ok((host, port));
        }

        let mut guard = self.codex_child.lock().await;

        // Re-probe under the lock so concurrent first-connects don't both
        // try to spawn.
        if TcpStream::connect((host.as_str(), port)).await.is_ok() {
            return Ok((host, port));
        }

        let env = self.daemon_launch_env().await;
        let child_alive = matches!(guard.as_mut().map(Child::try_wait), Some(Ok(None)));
        if !child_alive {
            let listen = format!("ws://{host}:{port}");
            let mut command = codex_command(&bin);
            apply_launch_env_to_command(&mut command, &env);
            let mut child = command
                .arg("app-server")
                .arg("--listen")
                .arg(&listen)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| {
                    format!("spawning `{} app-server --listen {listen}`", bin.display())
                })?;

            if let Some(stderr) = child.stderr.take() {
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        warn!(target: "codex", "{line}");
                    }
                });
            }

            *guard = Some(child);
        }
        drop(guard);

        // Poll the listener until it accepts a connection. Codex usually
        // binds within a few hundred milliseconds; 5s is generous.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpStream::connect((host.as_str(), port)).await.is_ok() {
                return Ok((host, port));
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "codex app-server did not start listening on {host}:{port} within 5s"
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn opencode_bridge_arc(&self) -> anyhow::Result<Arc<OpencodeBridge>> {
        let (bin, discoverable) = {
            let cfg = self.config.load();
            if !cfg.agents.opencode.enabled {
                return Err(anyhow!("opencode agent is disabled"));
            }
            (
                cfg.agents.opencode.bin.clone(),
                cfg.agents.opencode.discoverable,
            )
        };
        let bridge = self
            .opencode_bridge
            .get_or_try_init(|| async {
                // `OpencodeBridgeBuilder::from_env()` reads
                // `OPENCODE_BRIDGE_BIN` (and friends) at `build()` time; the
                // host config's `opencode.bin` overrides whatever the parent
                // shell set. Mirror the pre-A5 daemon behavior.
                unsafe {
                    std::env::set_var("OPENCODE_BRIDGE_BIN", &bin);
                    // `--discoverable` advertises this managed server in
                    // `~/.local/state/opencode/server.json` so local TUIs
                    // attach to it. Gated on host config so an old opencode
                    // binary without the flag is never handed it.
                    std::env::set_var(
                        "OPENCODE_BRIDGE_DISCOVERABLE",
                        if discoverable { "1" } else { "0" },
                    );
                }
                OpencodeBridge::builder()
                    .from_env()
                    .build()
                    .await
                    .context("initializing opencode bridge")
            })
            .await?;
        Ok(Arc::clone(bridge))
    }

    fn codex_available(&self) -> bool {
        let cfg = self.config.load();
        cfg.agents.codex.enabled && self.codex_available
    }

    async fn daemon_launch_env(&self) -> LaunchEnvironment {
        let cwd = std::env::current_dir().ok();
        self.launch_env.resolve(cwd.as_deref()).await
    }

    fn pi_available(&self, env: &LaunchEnvironment) -> bool {
        let cfg = self.config.load();
        cfg.agents.pi.enabled && resolve_pi_bin(&cfg.agents.pi.bin, env).is_some()
    }

    fn opencode_available(&self, env: &LaunchEnvironment) -> bool {
        let cfg = self.config.load();
        cfg.agents.opencode.enabled
            && (env_non_empty(env, "OPENCODE_BRIDGE_BACKEND_URL")
                || program_available(env, &cfg.agents.opencode.bin))
    }

    fn amp_available(&self, env: &LaunchEnvironment) -> bool {
        let cfg = self.config.load();
        cfg.agents.amp.enabled
            && program_available(env, &cfg.agents.amp.bin)
            && has_amp_auth(&cfg.agents.amp.api_key_env, env)
    }

    fn claude_available(&self, env: &LaunchEnvironment) -> bool {
        let cfg = self.config.load();
        cfg.agents.claude.enabled && program_available(env, &cfg.agents.claude.bin)
    }

    fn droid_available(&self, env: &LaunchEnvironment) -> bool {
        let cfg = self.config.load();
        cfg.agents.droid.enabled
            && program_available(env, &cfg.agents.droid.bin)
            && has_factory_auth(&cfg.agents.droid.api_key_env, env)
    }

    fn devin_available(&self, env: &LaunchEnvironment) -> bool {
        let cfg = self.config.load();
        cfg.agents.devin.enabled && program_available(env, &cfg.agents.devin.bin)
    }

    fn grok_available(&self, env: &LaunchEnvironment) -> bool {
        let cfg = self.config.load();
        cfg.agents.grok.enabled && program_available(env, &cfg.agents.grok.bin)
    }

    fn shell_available(&self) -> bool {
        let cfg = self.config.load();
        cfg.agents.shell.enabled && which::which(&cfg.agents.shell.shell_bin).is_ok()
    }

    async fn hermes_available(&self, env: &LaunchEnvironment) -> bool {
        let (enabled, bin, api_base) = {
            let cfg = self.config.load();
            (
                cfg.agents.hermes.enabled,
                cfg.agents.hermes.bin.clone(),
                cfg.agents.hermes.api_base.clone(),
            )
        };
        enabled && (program_available(env, &bin) || hermes_api_available(&api_base).await)
    }
}

async fn hermes_api_available(api_base: &str) -> bool {
    let url = format!("{}/health", api_base.trim_end_matches('/'));
    matches!(
        tokio::time::timeout(Duration::from_millis(300), reqwest::get(url)).await,
        Ok(Ok(response)) if response.status().is_success()
    )
}

async fn reap_codex_stream_child(mut child: Child, label: &'static str) {
    match tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
        Ok(Ok(_)) => return,
        Ok(Err(error)) => {
            warn!(target: "codex", "waiting for {label} child failed: {error}");
            return;
        }
        Err(_) => {}
    }

    terminate_codex_child(&mut child, label).await;
}

async fn terminate_codex_child(child: &mut Child, label: &'static str) {
    terminate_codex_child_tree(child, label).await;
    wait_for_codex_child_exit(child, label).await;
}

async fn terminate_codex_child_tree(child: &mut Child, label: &'static str) {
    #[cfg(not(windows))]
    let _ = label;

    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let mut taskkill = Command::new("taskkill.exe");
        taskkill
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide_windows_console(&mut taskkill);

        match tokio::time::timeout(Duration::from_secs(5), taskkill.status()).await {
            Ok(Ok(status)) if status.success() => return,
            Ok(Ok(status)) => {
                warn!(target: "codex", "{label} taskkill exited with {status}");
            }
            Ok(Err(error)) => {
                warn!(target: "codex", "failed to run taskkill for {label}: {error}");
            }
            Err(_) => {
                warn!(target: "codex", "timed out running taskkill for {label}");
            }
        }
    }

    let _ = child.kill().await;
}

async fn wait_for_codex_child_exit(child: &mut Child, label: &'static str) {
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(_)) | Ok(Err(_)) => {}
        Err(_) => warn!(target: "codex", "{label} child did not exit after termination"),
    }
}

fn codex_command(bin: &Path) -> Command {
    let mut command = {
        #[cfg(windows)]
        {
            if codex_needs_windows_cmd_shell(bin) {
                let shell = std::env::var_os("ComSpec")
                    .unwrap_or_else(|| std::ffi::OsString::from("cmd.exe"));
                let mut cmd = Command::new(shell);
                cmd.arg("/d").arg("/c").arg(bin);
                hide_windows_console(&mut cmd);
                cmd
            } else {
                let mut cmd = Command::new(bin);
                hide_windows_console(&mut cmd);
                cmd
            }
        }
        #[cfg(not(windows))]
        {
            Command::new(bin)
        }
    };

    // Pin every spawned `codex` invocation to $HOME so any long-lived
    // children (in particular `codex app-server --listen unix://` and
    // `codex app-server proxy`) never inherit a transient project
    // directory. Without this, a leaked daemon can stick the user
    // inside whatever dir alleycat happened to be invoked from when it
    // first spawned the codex child. See AGENTS.md ("orphan codex
    // daemon") for the failure mode this guards against.
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        command.current_dir(home);
    }

    command
}

#[cfg(windows)]
fn hide_windows_console(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

fn apply_launch_env_to_command(command: &mut Command, env: &LaunchEnvironment) {
    command.env_clear().envs(env.clone().into_pairs());
}

#[cfg_attr(not(test), allow(dead_code))]
fn codex_needs_windows_cmd_shell(bin: &Path) -> bool {
    let Some(file_name) = bin.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !file_name.eq_ignore_ascii_case("codex")
        && !file_name.to_ascii_lowercase().starts_with("codex.")
    {
        return false;
    }
    matches!(
        bin.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        None | Some("cmd" | "bat")
    )
}

/// Probe the user-installed Codex CLI. Prefer upstream daemon lifecycle plus
/// Unix proxy when available, then the legacy manual Unix proxy path, then the
/// older TCP websocket listener, and finally stdio for older CLIs. Any failure
/// (binary missing, exec error, garbled output) makes that candidate unavailable.
/// If no candidate can be spawned, we keep `Stdio` as the fallback mode but
/// report codex unavailable.
async fn detect_codex(bin: &str, env: &LaunchEnvironment) -> CodexDetection {
    let fallback_bin = PathBuf::from(bin);
    let candidates = {
        let mut resolved = Vec::new();
        if let Some(path) = env.find_on_path(bin) {
            resolved.push(path);
        }
        resolved.extend(program_candidates(Path::new(bin)));
        if resolved.is_empty() {
            vec![fallback_bin.clone()]
        } else {
            resolved.sort();
            resolved.dedup();
            resolved
        }
    };
    let candidates = newest_codex_candidates_first(candidates).await;

    for candidate in candidates {
        let mut command = codex_command(&candidate);
        apply_launch_env_to_command(&mut command, env);
        command.arg("app-server").arg("--help");
        let output = match tokio::time::timeout(Duration::from_secs(5), command.output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(err)) => {
                warn!(
                    error = %err,
                    configured_bin = %bin,
                    bin = %candidate.display(),
                    "codex app-server --help failed"
                );
                continue;
            }
            Err(_) => {
                warn!(
                    configured_bin = %bin,
                    bin = %candidate.display(),
                    "codex app-server --help timed out"
                );
                continue;
            }
        };
        if !output.status.success() {
            warn!(
                status = %output.status,
                configured_bin = %bin,
                bin = %candidate.display(),
                "codex app-server --help exited unsuccessfully"
            );
            continue;
        }
        let mut help = String::from_utf8_lossy(&output.stdout).into_owned();
        help.push_str(&String::from_utf8_lossy(&output.stderr));
        let listen_supported = help.contains("--listen");
        let proxy_supported = codex_app_server_proxy_supported(&candidate, env).await;
        // NOTE: We intentionally do NOT select `CodexMode::UnixDaemon`
        // even when upstream's `codex app-server daemon` lifecycle is
        // supported. That mode delegates daemon ownership to upstream
        // codex CLI, which double-forks and reparents the daemon to
        // `systemd --user`; the daemon then outlives alleycat
        // restarts, retains stale cwd / env, and silently traps the
        // user inside whatever directory the very first launch came
        // from. Prefer `UnixProxy` so the daemon runs as an alleycat
        // `kill_on_drop` child that dies with the alleycat process and
        // is respawned cleanly with $HOME cwd on next request. See
        // AGENTS.md ("orphan codex daemon").
        let mode = if listen_supported && proxy_supported {
            CodexMode::UnixProxy
        } else if listen_supported {
            CodexMode::Websocket
        } else {
            CodexMode::Stdio
        };
        return CodexDetection {
            mode,
            bin: candidate,
            available: true,
        };
    }

    CodexDetection {
        mode: CodexMode::Stdio,
        bin: fallback_bin,
        available: false,
    }
}

async fn codex_app_server_proxy_supported(bin: &Path, env: &LaunchEnvironment) -> bool {
    let mut command = codex_command(bin);
    apply_launch_env_to_command(&mut command, env);
    command
        .arg("app-server")
        .arg("proxy")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    matches!(
        tokio::time::timeout(Duration::from_secs(5), command.status()).await,
        Ok(Ok(status)) if status.success()
    )
}

// `codex app-server daemon` is still useful for SSH/remote deploys
// where the daemon must outlive the alleycat connection. Local
// alleycat now ignores this probe so the codex daemon is always
// alleycat's child (`UnixProxy`). Kept for reference / future
// remote-bridge support; underscore-prefixed so dead-code-lint stays
// clean if the function is otherwise unused.
#[allow(dead_code)]
async fn _codex_app_server_daemon_supported(bin: &Path, env: &LaunchEnvironment) -> bool {
    let mut command = codex_command(bin);
    apply_launch_env_to_command(&mut command, env);
    command
        .arg("app-server")
        .arg("daemon")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    matches!(
        tokio::time::timeout(Duration::from_secs(5), command.status()).await,
        Ok(Ok(status)) if status.success()
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexDaemonOutput {
    socket_path: Option<PathBuf>,
}

async fn run_codex_app_server_daemon(
    bin: &Path,
    subcommand: &str,
    env: &LaunchEnvironment,
) -> anyhow::Result<CodexDaemonOutput> {
    let mut command = codex_command(bin);
    apply_launch_env_to_command(&mut command, env);
    command.arg("app-server").arg("daemon").arg(subcommand);
    let output = tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .with_context(|| {
            format!(
                "timed out running `{} app-server daemon {subcommand}`",
                bin.display()
            )
        })?
        .with_context(|| format!("running `{} app-server daemon {subcommand}`", bin.display()))?;

    if !output.status.success() {
        return Err(anyhow!(
            "`{} app-server daemon {subcommand}` exited with {}: stdout={} stderr={}",
            bin.display(),
            output.status,
            process_output_excerpt(&output.stdout),
            process_output_excerpt(&output.stderr),
        ));
    }

    serde_json::from_slice::<CodexDaemonOutput>(&output.stdout).with_context(|| {
        format!(
            "parsing `{} app-server daemon {subcommand}` JSON output: {}",
            bin.display(),
            process_output_excerpt(&output.stdout)
        )
    })
}

fn process_output_excerpt(bytes: &[u8]) -> String {
    const MAX_OUTPUT_LEN: usize = 2000;
    let value = String::from_utf8_lossy(bytes).trim().replace('\n', "\\n");
    if value.len() <= MAX_OUTPUT_LEN {
        return value;
    }
    let excerpt = value.chars().take(MAX_OUTPUT_LEN).collect::<String>();
    format!("{excerpt}...")
}

async fn probe_codex_app_server_proxy(
    bin: &Path,
    socket_path: Option<&Path>,
    env: &LaunchEnvironment,
) -> anyhow::Result<()> {
    let mut command = codex_command(bin);
    apply_launch_env_to_command(&mut command, env);
    command.arg("app-server").arg("proxy");
    if let Some(socket_path) = socket_path {
        command.arg("--sock").arg(socket_path);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            let socket = socket_path
                .map(|path| format!(" --sock {}", path.display()))
                .unwrap_or_default();
            format!("spawning `{} app-server proxy{socket}`", bin.display())
        })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("app-server proxy child missing stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("app-server proxy child missing stdout"))?;
    let stderr = child.stderr.take();

    if let Some(stderr) = stderr {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                warn!(target: "codex", "{line}");
            }
        });
    }

    let child_io = tokio::io::join(stdout, stdin);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::client_async("ws://codex-app-server-proxy.localhost/rpc", child_io),
    )
    .await;

    let result = match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(error).context("codex app-server proxy websocket handshake failed"),
        Err(_) => Err(anyhow!(
            "timed out opening websocket over codex app-server proxy"
        )),
    };
    terminate_codex_child(&mut child, "app-server proxy probe").await;
    result
}

#[cfg(unix)]
async fn default_codex_control_socket_accepts_connections(
    env: &LaunchEnvironment,
) -> Option<PathBuf> {
    let path = default_codex_control_socket_path(env)?;
    UnixStream::connect(&path).await.ok()?;
    Some(path)
}

#[cfg(not(unix))]
async fn default_codex_control_socket_accepts_connections(
    _env: &LaunchEnvironment,
) -> Option<PathBuf> {
    None
}

fn alleycat_codex_control_socket_path(default_socket_path: &Path) -> PathBuf {
    default_socket_path
        .parent()
        .map(|parent| parent.join("alleycat-app-server-control.sock"))
        .unwrap_or_else(|| default_socket_path.with_file_name("alleycat-app-server-control.sock"))
}

fn codex_remote_control_socket_path(
    endpoint: &CodexUnixEndpoint,
    env: &LaunchEnvironment,
) -> Option<PathBuf> {
    endpoint
        .socket_path
        .clone()
        .or_else(|| default_codex_control_socket_path(env))
}

fn codex_auth_path(env: &LaunchEnvironment) -> Option<PathBuf> {
    Some(codex_home_path(env)?.join("auth.json"))
}

#[cfg(unix)]
fn default_codex_control_socket_path(env: &LaunchEnvironment) -> Option<PathBuf> {
    let codex_home = codex_home_path(env)?;
    Some(
        codex_home
            .join("app-server-control")
            .join("app-server-control.sock"),
    )
}

fn codex_home_path(env: &LaunchEnvironment) -> Option<PathBuf> {
    match env_path(env, "CODEX_HOME") {
        Some(path) => {
            if !path.is_dir() {
                return None;
            }
            path.canonicalize().ok()
        }
        None => Some(directories::BaseDirs::new()?.home_dir().join(".codex")),
    }
}

fn env_path(env: &LaunchEnvironment, key: &str) -> Option<PathBuf> {
    env.get(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn pi_program_aliases() -> [(OsString, Vec<OsString>); 2] {
    [
        (
            OsString::from("pi"),
            vec![OsString::from("pi-coding-agent")],
        ),
        (
            OsString::from("pi-coding-agent"),
            vec![OsString::from("pi")],
        ),
    ]
}

/// Resolve the configured pi binary. If the configured name
/// isn't on PATH, fall back to known aliases (`pi`, `pi-coding-agent`) so
/// users with stale config or non-canonical install layouts still get the
/// agent reported as available and spawn against a binary that actually
/// exists. Returns the resolved name (the one that should be invoked).
fn resolve_pi_bin(configured: &str, env: &LaunchEnvironment) -> Option<PathBuf> {
    if let Some(path) = resolve_program(configured, env) {
        return Some(path);
    }
    for alias in ["pi", "pi-coding-agent"] {
        if alias != configured
            && let Some(path) = resolve_program(alias, env)
        {
            return Some(path);
        }
    }
    None
}

async fn drain_desktop_router_session(session: Arc<Session>, out_tx: mpsc::UnboundedSender<Value>) {
    let attach = session.install_attachment(None);
    for sequenced in attach.backlog {
        session.note_drainer_attempt(sequenced.seq);
        if out_tx.send(sequenced.payload).is_err() {
            return;
        }
    }
    if let Some(payload) = attach.replay_redelivery
        && out_tx.send(payload).is_err()
    {
        return;
    }
    let mut live_rx = attach.live_rx;
    while let Some(sequenced) = live_rx.recv().await {
        session.note_drainer_attempt(sequenced.seq);
        if out_tx.send(sequenced.payload).is_err() {
            break;
        }
    }
}

fn desktop_router_agent_from_uri(uri: &impl std::fmt::Display) -> Option<String> {
    let raw = uri.to_string();
    let (path, query) = raw
        .split_once('?')
        .map_or((raw.as_str(), None), |(path, query)| (path, Some(query)));
    if let Some(agent) = query.and_then(agent_from_query) {
        return Some(agent);
    }
    let path = path.trim_matches('/');
    let agent = path
        .strip_prefix("agent/")
        .or_else(|| path.strip_prefix("agents/"))
        .unwrap_or(path);
    sanitize_desktop_agent(agent)
}

fn agent_from_query(query: &str) -> Option<String> {
    query.split('&').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        if key == "agent" {
            sanitize_desktop_agent(value)
        } else {
            None
        }
    })
}

fn sanitize_desktop_agent(agent: &str) -> Option<String> {
    let agent = agent.trim().trim_matches('/');
    if agent.is_empty() || agent == "codex" {
        return None;
    }
    let valid = agent
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    valid.then(|| agent.replace('_', "-"))
}

fn desktop_router_stub_response(method: &str) -> Option<Value> {
    match method {
        "app/list"
        | "plugin/list"
        | "plugin/share/list"
        | "marketplace/list"
        | "thread/loaded/list"
        | "experimentalFeature/list"
        | "collaborationMode/list" => Some(json!({"data":[],"nextCursor":null})),
        "hooks/list" => Some(json!({"data":[],"hooks":[],"nextCursor":null})),
        "thread/goal/get" => Some(json!({"goal":null})),
        "thread/backgroundTerminals/clean"
        | "config/mcpServer/reload"
        | "experimentalFeature/enablement/set"
        | "marketplace/add"
        | "plugin/install"
        | "account/login/start"
        | "account/login/cancel"
        | "account/logout"
        | "feedback/upload" => Some(json!({})),
        "account/rateLimits/read" => Some(json!({"rateLimits":[]})),
        "getAuthStatus" => Some(json!({
            "authMethod": "apikey",
            "authToken": null,
            "requiresAuth": false,
            "email": null,
            "planAtLogin": null
        })),
        _ => None,
    }
}

fn agent_kind_from_str(name: &str) -> Option<AgentKind> {
    match name {
        "pi" => Some(AgentKind::Pi),
        "amp" => Some(AgentKind::Amp),
        "claude" => Some(AgentKind::Claude),
        "opencode" => Some(AgentKind::Opencode),
        "droid" => Some(AgentKind::Droid),
        "hermes" => Some(AgentKind::Hermes),
        "devin" => Some(AgentKind::Devin),
        "grok" => Some(AgentKind::Grok),
        "shell" => Some(AgentKind::Shell),
        _ => None,
    }
}

fn agent_kind_str(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Pi => "pi",
        AgentKind::Amp => "amp",
        AgentKind::Claude => "claude",
        AgentKind::Opencode => "opencode",
        AgentKind::Droid => "droid",
        AgentKind::Hermes => "hermes",
        AgentKind::Devin => "devin",
        AgentKind::Grok => "grok",
        AgentKind::Shell => "shell",
    }
}

impl crate::config::AgentsConfig {
    fn is_enabled(&self, kind: AgentKind) -> bool {
        match kind {
            AgentKind::Pi => self.pi.enabled,
            AgentKind::Amp => self.amp.enabled,
            AgentKind::Claude => self.claude.enabled,
            AgentKind::Opencode => self.opencode.enabled,
            AgentKind::Droid => self.droid.enabled,
            AgentKind::Hermes => self.hermes.enabled,
            AgentKind::Devin => self.devin.enabled,
            AgentKind::Grok => self.grok.enabled,
            AgentKind::Shell => self.shell.enabled,
        }
    }
}

fn program_available(env: &LaunchEnvironment, configured: &str) -> bool {
    resolve_program(configured, env).is_some()
}

fn resolve_program(configured: &str, env: &LaunchEnvironment) -> Option<PathBuf> {
    let path = Path::new(configured);
    if path.components().count() > 1 {
        return is_executable_file(path).then_some(path.to_path_buf());
    }
    env.find_on_path(configured)
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn env_non_empty(env: &LaunchEnvironment, key: &str) -> bool {
    env.get(key).is_some_and(|value| !value.is_empty())
}

fn has_factory_auth(api_key_env: &str, env: &LaunchEnvironment) -> bool {
    if env.get(api_key_env).is_some() {
        return true;
    }
    let Some(home) = env_path(env, "HOME") else {
        return false;
    };
    let factory_dir = home.join(".factory");
    factory_dir.join("auth.encrypted").is_file()
        || (factory_dir.join("auth.v2.file").is_file() && factory_dir.join("auth.v2.key").is_file())
}

fn has_amp_auth(api_key_env: &str, env: &LaunchEnvironment) -> bool {
    if env.get(api_key_env).is_some() {
        return true;
    }
    let Some(home) = env_path(env, "HOME") else {
        return false;
    };
    let data_home = env_path(env, "XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
    data_home.join("amp/secrets.json").is_file() || home.join(".amp/oauth").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_auth_accepts_v2_store() {
        let home = crate::test_support::TempHome::new();
        let factory_dir = home.path().join(".factory");
        std::fs::create_dir_all(&factory_dir).unwrap();

        let api_key_env = "ALLEYCAT_TEST_FACTORY_API_KEY_UNSET";
        unsafe { std::env::remove_var(api_key_env) };

        assert!(!has_factory_auth(
            api_key_env,
            &LaunchEnvironment::current()
        ));

        std::fs::write(factory_dir.join("auth.v2.file"), b"auth").unwrap();
        assert!(!has_factory_auth(
            api_key_env,
            &LaunchEnvironment::current()
        ));

        std::fs::write(factory_dir.join("auth.v2.key"), b"key").unwrap();
        assert!(has_factory_auth(api_key_env, &LaunchEnvironment::current()));
    }

    #[test]
    fn resolved_environment_controls_program_availability() {
        let mut home = crate::test_support::TempHome::new();
        let bin_dir = home.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let agent_bin = bin_dir.join("agent");
        std::fs::write(&agent_bin, b"#!/bin/sh\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&agent_bin).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&agent_bin, perms).unwrap();
        }

        home.override_env(&[("PATH", bin_dir.to_str().unwrap())]);
        let env = LaunchEnvironment::current();

        assert!(program_available(&env, "agent"));
        assert_eq!(resolve_pi_bin("pi", &env), None);
    }

    #[test]
    fn codex_windows_cmd_shell_detection_is_limited_to_codex_shims() {
        assert!(codex_needs_windows_cmd_shell(Path::new("codex")));
        assert!(codex_needs_windows_cmd_shell(Path::new("codex.cmd")));
        assert!(codex_needs_windows_cmd_shell(Path::new("CODEX.BAT")));
        assert!(!codex_needs_windows_cmd_shell(Path::new("codex.exe")));
        assert!(!codex_needs_windows_cmd_shell(Path::new("pi.cmd")));
    }
}
