//! Browser-native HTTP/WebSocket adapter for the Alleycat host protocol.
//!
//! The iroh transport remains the canonical remote protocol. This module is a
//! small local adapter for browser clients that cannot dial iroh QUIC: it
//! exposes loopback HTTP routes for status/pairing and a WebSocket endpoint
//! whose binary messages carry the same `u32be length + JSON` frames used by
//! the iroh stream handshake.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, anyhow};
use arc_swap::ArcSwap;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::{Sink, SinkExt, Stream, StreamExt};
use iroh::SecretKey;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, tungstenite};
use tracing::{debug, info, warn};

use crate::agents::AgentManager;
use crate::config::HostConfig;
use crate::daemon::control::{StatusInfo, token_fingerprint};
use crate::framing::{MAX_FRAME_BYTES, read_json_frame};
use crate::host;
use crate::paths;
use crate::protocol::{Request, Response, Resume, SessionInfo};

/// Configuration for the local HTTP/WebSocket adapter.
///
/// The default serving mode is conservative: bind loopback only, do not expose
/// static files unless the caller provides a directory, and only return
/// `/api/pair` to loopback clients. Tailnet auto-pair is explicit via
/// `auto_pair_tailnet`.
#[derive(Clone, Debug)]
pub struct HttpServerConfig {
    pub listen: SocketAddr,
    pub bundle_dir: Option<PathBuf>,
    pub ws_only: bool,
    pub auto_pair_tailnet: bool,
}

impl Default for HttpServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 5851)),
            bundle_dir: None,
            ws_only: false,
            auto_pair_tailnet: false,
        }
    }
}

/// Shared daemon state required by the HTTP adapter.
///
/// This intentionally mirrors the iroh/control paths without owning them. The
/// caller keeps the daemon lifetime and shutdown orchestration; the HTTP task
/// borrows cloned `Arc` handles and exits when `shutdown` fires.
#[derive(Clone)]
pub struct HttpServerState {
    pub config: Arc<ArcSwap<HostConfig>>,
    pub agents: AgentManager,
    pub secret_key: SecretKey,
    pub node_id: String,
    pub started_at: std::time::Instant,
    pub binary_version: String,
}

/// Serve `/api/status`, `/api/pair`, `/ws`, and optional static files.
///
/// The WebSocket route accepts binary frames only. Each binary message must be
/// one complete Alleycat frame (`u32be` length prefix followed by JSON). Text,
/// malformed, oversized, bad-origin, and bad-token inputs are rejected without
/// logging the raw token.
pub async fn serve_http(
    listener: TcpListener,
    state: HttpServerState,
    server_config: HttpServerConfig,
    shutdown: Arc<Notify>,
) -> anyhow::Result<()> {
    info!(addr = %listener.local_addr()?, "http adapter listening");
    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => {
                info!("http adapter received shutdown");
                break;
            }
            accepted = listener.accept() => {
                let (stream, peer_addr) = accepted.context("accepting http connection")?;
                let state = state.clone();
                let server_config = server_config.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, peer_addr, state, server_config).await {
                        debug!("http connection ended: {error:#}");
                    }
                });
            }
        }
    }
    Ok(())
}

async fn handle_connection(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    state: HttpServerState,
    server_config: HttpServerConfig,
) -> anyhow::Result<()> {
    let request = read_http_request(&mut stream).await?;
    if !origin_allowed(
        request.headers.get("origin").map(String::as_str),
        &server_config,
    ) {
        write_response(&mut stream, 403, "Forbidden", "text/plain", b"forbidden").await?;
        return Ok(());
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/status") if !server_config.ws_only => {
            let body =
                serde_json::to_vec_pretty(&status_info(&state, Some(server_config.listen)).await?)?;
            write_response(&mut stream, 200, "OK", "application/json", &body).await?;
        }
        ("GET", "/api/pair") if !server_config.ws_only => {
            if !pair_allowed(peer_addr, &server_config) {
                write_response(&mut stream, 403, "Forbidden", "text/plain", b"forbidden").await?;
                return Ok(());
            }
            let cfg = state.config.load();
            let payload = host::pair_payload(&state.secret_key, &cfg, None);
            let body = serde_json::to_vec(&payload)?;
            write_response(&mut stream, 200, "OK", "application/json", &body).await?;
        }
        ("GET", "/ws") => {
            upgrade_websocket(stream, request, peer_addr, state).await?;
        }
        ("GET", path) if !server_config.ws_only => {
            if let Some(bundle_dir) = &server_config.bundle_dir {
                serve_static(&mut stream, bundle_dir, path).await?;
            } else {
                write_response(&mut stream, 404, "Not Found", "text/plain", b"not found").await?;
            }
        }
        _ => {
            write_response(&mut stream, 404, "Not Found", "text/plain", b"not found").await?;
        }
    }

    Ok(())
}

async fn upgrade_websocket(
    mut stream: TcpStream,
    request: HttpRequest,
    peer_addr: SocketAddr,
    state: HttpServerState,
) -> anyhow::Result<()> {
    let key = request
        .headers
        .get("sec-websocket-key")
        .ok_or_else(|| anyhow!("missing sec-websocket-key"))?;
    let accept = websocket_accept_key(key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\
         \r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    let websocket =
        WebSocketStream::from_raw_socket(stream, tungstenite::protocol::Role::Server, None).await;
    handle_ws(websocket, peer_addr, state).await
}

async fn handle_ws(
    mut websocket: WebSocketStream<TcpStream>,
    peer_addr: SocketAddr,
    state: HttpServerState,
) -> anyhow::Result<()> {
    while let Some(message) = websocket.next().await {
        match message? {
            Message::Binary(bytes) => {
                if bytes.len() > MAX_FRAME_BYTES + 4 {
                    websocket
                        .close(Some(tungstenite::protocol::CloseFrame {
                            code: tungstenite::protocol::frame::coding::CloseCode::Size,
                            reason: "protocol violation: oversized frame".into(),
                        }))
                        .await?;
                    return Ok(());
                }
                let action = dispatch_ws_frame(&bytes, peer_addr, &state).await;
                let response = action.response;
                let frame = encode_response_frame(&response)?;
                websocket.send(Message::Binary(frame.into())).await?;
                if let Some(connect) = action.connect {
                    return serve_connected_ws(websocket, connect).await;
                }
            }
            Message::Text(_) => {
                websocket
                    .close(Some(tungstenite::protocol::CloseFrame {
                        code: tungstenite::protocol::frame::coding::CloseCode::Protocol,
                        reason: "protocol violation: text frame".into(),
                    }))
                    .await?;
                return Ok(());
            }
            Message::Close(_) => return Ok(()),
            Message::Ping(payload) => websocket.send(Message::Pong(payload)).await?,
            Message::Pong(_) => {}
            Message::Frame(_) => {}
        }
    }
    Ok(())
}

async fn dispatch_ws_frame(
    bytes: &[u8],
    peer_addr: SocketAddr,
    state: &HttpServerState,
) -> WsDispatch {
    let mut cursor = std::io::Cursor::new(bytes);
    let request = match read_json_frame::<Request, _>(&mut cursor).await {
        Ok(request) => request,
        Err(_) => return WsDispatch::response(Response::error("malformed frame")),
    };

    let token = state.config.load().token.clone();
    if request.token() != token {
        warn!(peer = %peer_addr, "rejecting websocket frame: invalid token");
        return WsDispatch::response(Response::error("invalid token"));
    }

    match request {
        Request::ListAgents { .. } => {
            WsDispatch::response(Response::agents(state.agents.list_agents().await))
        }
        Request::RestartAgent { agent, .. } => match state.agents.restart_agent(&agent).await {
            Ok(()) => WsDispatch::response(Response::ok()),
            Err(error) => WsDispatch::response(Response::error(error.to_string())),
        },
        Request::Connect { agent, resume, .. } => {
            if !state.agents.agent_enabled(&agent) {
                return WsDispatch::response(Response::error(format!(
                    "agent `{agent}` is disabled or unknown"
                )));
            }
            let Some(agent_id) = AgentManager::agent_id(&agent) else {
                return WsDispatch::response(Response::error(format!(
                    "agent `{agent}` is unknown"
                )));
            };
            let last_seen = resume.as_ref().map(|r: &Resume| r.last_seq);
            let resolved = state.agents.session_registry().resolve_attach(
                peer_addr.to_string(),
                agent_id,
                last_seen,
            );
            let session = SessionInfo {
                attached: resolved.kind.into(),
                current_seq: resolved.current_seq,
                floor_seq: resolved.floor_seq,
            };
            WsDispatch {
                response: Response::ok_with_session(session),
                connect: Some(WsConnect {
                    agents: state.agents.clone(),
                    agent,
                    session: resolved.session,
                    last_seen,
                }),
            }
        }
    }
}

struct WsDispatch {
    response: Response,
    connect: Option<WsConnect>,
}

impl WsDispatch {
    fn response(response: Response) -> Self {
        Self {
            response,
            connect: None,
        }
    }
}

struct WsConnect {
    agents: AgentManager,
    agent: String,
    session: Arc<alleycat_bridge_core::Session>,
    last_seen: Option<u64>,
}

async fn serve_connected_ws(
    websocket: WebSocketStream<TcpStream>,
    connect: WsConnect,
) -> anyhow::Result<()> {
    let stream = WebSocketBinaryStream::new(websocket);
    connect
        .agents
        .serve_bridge_agent_with_session(&connect.agent, stream, connect.session, connect.last_seen)
        .await
}

struct WebSocketBinaryStream {
    websocket: WebSocketStream<TcpStream>,
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
}

impl WebSocketBinaryStream {
    fn new(websocket: WebSocketStream<TcpStream>) -> Self {
        Self {
            websocket,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
        }
    }
}

impl AsyncRead for WebSocketBinaryStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if !self.read_buf.is_empty() {
                let n = self.read_buf.len().min(buf.remaining());
                buf.put_slice(&self.read_buf[..n]);
                self.read_buf.drain(..n);
                return Poll::Ready(Ok(()));
            }

            match Pin::new(&mut self.websocket).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(Message::Binary(bytes)))) => {
                    if bytes.len() > MAX_FRAME_BYTES + 4 {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "websocket binary frame too large",
                        )));
                    }
                    let body = match decode_ws_binary_frame(&bytes) {
                        Ok(body) => body,
                        Err(error) => return Poll::Ready(Err(error)),
                    };
                    self.read_buf.extend_from_slice(body);
                    self.read_buf.push(b'\n');
                }
                Poll::Ready(Some(Ok(Message::Close(_)))) | Poll::Ready(None) => {
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(Ok(Message::Ping(_))))
                | Poll::Ready(Some(Ok(Message::Pong(_)))) => {
                    continue;
                }
                Poll::Ready(Some(Ok(Message::Text(_))))
                | Poll::Ready(Some(Ok(Message::Frame(_)))) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "websocket protocol violation",
                    )));
                }
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Err(std::io::Error::other(error)));
                }
            }
        }
    }
}

impl AsyncWrite for WebSocketBinaryStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.write_buf.extend_from_slice(buf);
        loop {
            let Some(newline) = self.write_buf.iter().position(|byte| *byte == b'\n') else {
                return Poll::Ready(Ok(buf.len()));
            };
            let mut line = self.write_buf.drain(..=newline).collect::<Vec<u8>>();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }

            match Pin::new(&mut self.websocket).poll_ready(cx) {
                Poll::Pending => {
                    self.write_buf
                        .splice(0..0, line.into_iter().chain(std::iter::once(b'\n')));
                    return Poll::Pending;
                }
                Poll::Ready(Err(error)) => return Poll::Ready(Err(std::io::Error::other(error))),
                Poll::Ready(Ok(())) => {
                    let frame = match encode_ws_binary_frame(&line) {
                        Ok(frame) => frame,
                        Err(error) => return Poll::Ready(Err(error)),
                    };
                    if let Err(error) =
                        Pin::new(&mut self.websocket).start_send(Message::Binary(frame.into()))
                    {
                        return Poll::Ready(Err(std::io::Error::other(error)));
                    }
                }
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        if !self.write_buf.is_empty() {
            match Pin::new(&mut self.websocket).poll_ready(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(std::io::Error::other(error))),
                Poll::Ready(Ok(())) => {
                    let line = std::mem::take(&mut self.write_buf);
                    let frame = match encode_ws_binary_frame(line.trim_ascii_end()) {
                        Ok(frame) => frame,
                        Err(error) => return Poll::Ready(Err(error)),
                    };
                    if let Err(error) =
                        Pin::new(&mut self.websocket).start_send(Message::Binary(frame.into()))
                    {
                        return Poll::Ready(Err(std::io::Error::other(error)));
                    }
                }
            }
        }
        Pin::new(&mut self.websocket)
            .poll_flush(cx)
            .map_err(std::io::Error::other)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.websocket)
            .poll_close(cx)
            .map_err(std::io::Error::other)
    }
}

fn decode_ws_binary_frame(bytes: &[u8]) -> std::io::Result<&[u8]> {
    if bytes.len() < 4 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "websocket frame missing length prefix",
        ));
    }
    let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if len > MAX_FRAME_BYTES || bytes.len() != len + 4 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "websocket frame length mismatch",
        ));
    }
    Ok(&bytes[4..])
}

fn encode_ws_binary_frame(body: &[u8]) -> std::io::Result<Vec<u8>> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "websocket frame too large",
        ));
    }
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);
    Ok(frame)
}

async fn status_info(
    state: &HttpServerState,
    http_endpoint: Option<SocketAddr>,
) -> anyhow::Result<StatusInfo> {
    let cfg = state.config.load();
    Ok(StatusInfo {
        pid: std::process::id(),
        node_id: state.node_id.clone(),
        token_short: token_fingerprint(&cfg.token),
        relay: cfg.relay.clone(),
        config_path: paths::host_config_file()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string()),
        uptime_secs: state.started_at.elapsed().as_secs(),
        agents: state.agents.list_agents().await,
        version: Some(state.binary_version.clone()),
        http_endpoint: http_endpoint.map(|addr| format!("http://{addr}")),
    })
}

fn encode_response_frame(response: &Response) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::to_vec(response)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(anyhow!("frame too large: {} bytes", body.len()));
    }
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

async fn read_http_request(stream: &mut TcpStream) -> anyhow::Result<HttpRequest> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(anyhow!("connection closed before request"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return Err(anyhow!("http header too large"));
        }
    }

    parse_http_request(&buf)
}

fn parse_http_request(buf: &[u8]) -> anyhow::Result<HttpRequest> {
    let text = std::str::from_utf8(buf).context("decoding http request")?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow!("missing method"))?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| anyhow!("missing path"))?
        .to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok(HttpRequest {
        method,
        path,
        headers,
    })
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

async fn serve_static(
    stream: &mut TcpStream,
    bundle_dir: &PathBuf,
    path: &str,
) -> anyhow::Result<()> {
    let relative = if path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };
    if relative.contains("..") {
        write_response(stream, 403, "Forbidden", "text/plain", b"forbidden").await?;
        return Ok(());
    }
    let full_path = bundle_dir.join(relative);
    match tokio::fs::read(&full_path).await {
        Ok(body) => write_response(stream, 200, "OK", content_type(relative), &body).await?,
        Err(_) => write_response(stream, 404, "Not Found", "text/plain", b"not found").await?,
    }
    Ok(())
}

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html"
    } else if path.ends_with(".js") {
        "text/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".json") || path.ends_with(".webmanifest") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

fn pair_allowed(peer_addr: SocketAddr, config: &HttpServerConfig) -> bool {
    peer_addr.ip().is_loopback() || (config.auto_pair_tailnet && !config.listen.ip().is_loopback())
}

fn origin_allowed(origin: Option<&str>, config: &HttpServerConfig) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    if origin == format!("http://{}", config.listen)
        || origin == format!("https://{}", config.listen)
    {
        return true;
    }
    origin.starts_with("http://127.0.0.1:")
        || origin.starts_with("http://localhost:")
        || (config.auto_pair_tailnet && origin.starts_with("https://"))
}

fn websocket_accept_key(key: &str) -> String {
    let mut sha1 = Sha1::new();
    sha1.update(key.as_bytes());
    sha1.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    BASE64.encode(sha1.finalize())
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn websocket_accept_key_matches_rfc_example() {
        assert_eq!(
            websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn origin_rejects_unlisted_cross_origin() {
        let cfg = HttpServerConfig::default();
        assert!(!origin_allowed(Some("https://evil.example"), &cfg));
        assert!(origin_allowed(Some("http://127.0.0.1:5173"), &cfg));
    }

    #[test]
    fn pair_is_loopback_only_unless_tailnet_auto_pair_enabled() {
        let cfg = HttpServerConfig::default();
        let loopback = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), 50000);
        let remote = SocketAddr::new(IpAddr::from([100, 64, 0, 10]), 50000);
        assert!(pair_allowed(loopback, &cfg));
        assert!(!pair_allowed(remote, &cfg));
    }
}
