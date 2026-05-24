use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio_tungstenite::tungstenite::Message;

use crate::jsonrpc::InboundMessage;

#[derive(Debug, Error)]
pub(crate) enum TransportError {
    #[error("connecting to {0}: {1}")]
    Connect(String, std::io::Error),
    #[error("websocket handshake: {0}")]
    Handshake(tokio_tungstenite::tungstenite::Error),
    #[error("websocket receive: {0}")]
    Receive(tokio_tungstenite::tungstenite::Error),
    #[error("websocket send: {0}")]
    Send(tokio_tungstenite::tungstenite::Error),
    #[error("websocket closed")]
    Closed,
    #[error("unsupported websocket frame")]
    UnsupportedFrame,
    #[error("parsing json-rpc frame: {0}")]
    Parse(serde_json::Error),
    #[error("serializing json-rpc frame: {0}")]
    Serialize(serde_json::Error),
    #[cfg(not(unix))]
    #[error("codex remote control requires Unix sockets")]
    UnsupportedPlatform,
}

#[cfg(unix)]
type Inner = tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>;

pub(crate) struct JsonRpcTransport {
    #[cfg(unix)]
    inner: Inner,
}

impl JsonRpcTransport {
    #[cfg(unix)]
    pub(crate) async fn connect(socket_path: &std::path::Path) -> Result<Self, TransportError> {
        let stream = tokio::net::UnixStream::connect(socket_path)
            .await
            .map_err(|source| TransportError::Connect(socket_path.display().to_string(), source))?;
        let (inner, _response) =
            tokio_tungstenite::client_async("ws://codex-app-server.localhost/rpc", stream)
                .await
                .map_err(TransportError::Handshake)?;
        Ok(Self { inner })
    }

    #[cfg(not(unix))]
    pub(crate) async fn connect(_socket_path: &std::path::Path) -> Result<Self, TransportError> {
        Err(TransportError::UnsupportedPlatform)
    }

    pub(crate) async fn send<T: Serialize>(&mut self, message: &T) -> Result<(), TransportError> {
        let text = serde_json::to_string(message).map_err(TransportError::Serialize)?;
        self.send_text(text).await
    }

    #[cfg(unix)]
    pub(crate) async fn send_text(&mut self, text: String) -> Result<(), TransportError> {
        self.inner
            .send(Message::Text(text.into()))
            .await
            .map_err(TransportError::Send)
    }

    #[cfg(not(unix))]
    pub(crate) async fn send_text(&mut self, _text: String) -> Result<(), TransportError> {
        Err(TransportError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    pub(crate) async fn read(&mut self) -> Result<InboundMessage, TransportError> {
        loop {
            let Some(message) = self.inner.next().await else {
                return Err(TransportError::Closed);
            };
            let message = message.map_err(TransportError::Receive)?;
            match message {
                Message::Text(text) => {
                    let value: Value =
                        serde_json::from_str(text.as_ref()).map_err(TransportError::Parse)?;
                    return InboundMessage::from_value(value).map_err(TransportError::Parse);
                }
                Message::Binary(bytes) => {
                    let value: Value =
                        serde_json::from_slice(bytes.as_ref()).map_err(TransportError::Parse)?;
                    return InboundMessage::from_value(value).map_err(TransportError::Parse);
                }
                Message::Ping(bytes) => {
                    self.inner
                        .send(Message::Pong(bytes))
                        .await
                        .map_err(TransportError::Send)?;
                }
                Message::Pong(_) => {}
                Message::Close(_) => return Err(TransportError::Closed),
                Message::Frame(_) => return Err(TransportError::UnsupportedFrame),
            }
        }
    }

    #[cfg(not(unix))]
    pub(crate) async fn read(&mut self) -> Result<InboundMessage, TransportError> {
        Err(TransportError::UnsupportedPlatform)
    }
}
