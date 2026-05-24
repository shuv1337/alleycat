//! Native supervisor for Codex app-server remote control.
//!
//! This crate owns the Codex control-socket JSON-RPC loop. Alleycat only tells
//! it which app-server socket and auth file belong to the currently managed
//! Codex child, then reads back a redacted status snapshot for diagnostics.

mod auth;
mod jsonrpc;
mod status;
mod supervisor;
mod transport;

pub use status::{CodexRemoteControlSnapshot, CodexRemoteControlState};
pub use supervisor::{
    CodexRemoteControlConfig, CodexRemoteControlHandle, CodexRemoteControlTuning,
};
