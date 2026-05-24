# Facts

- T2 may proceed now because T1 has been stable in local testing, but T1 remains the rollback path until native T2 is verified.
- The native remote-control implementation lives in a new workspace crate, initially `crates/codex-remote-control/`, to keep most T2 logic isolated from future upstream conflicts in `crates/alleycat/src/agents.rs`.
- `crates/alleycat/src/agents.rs` only wires lifecycle: it starts the native remote-control task when the Codex Unix app-server endpoint is ready, stops or restarts it with the Codex app-server child, and exposes its current status.
- T2 preserves the existing orphan-daemon policy: it does not re-enable or depend on `CodexMode::UnixDaemon`; the native task supervises the Alleycat-owned Codex UnixProxy app-server path.
- The new crate connects to the Codex app-server control socket as WebSocket-over-Unix-socket, performs `initialize` then `initialized`, consumes `remoteControl/status/changed`, and calls `remoteControl/enable` when status is disabled, errored, missing, or stale.
- The native implementation handles `account/chatgptAuthTokens/refresh` by reading current ChatGPT token data from `~/.codex/auth.json` without logging token values.
- The native implementation does not invent an attestation handshake; if Codex app-server sends an unexpected `attestation/generate` request, T2 records a blocked/error status with exact evidence.
- `alleycat status --json` exposes a concise Codex remote-control status object with state, server name, environment id, last update time, last enable reason, and error or blocked summary, without exposing token material.
- The existing `codex-rc-keeper.service` remains installed during T2; it is stopped and disabled only after the native task is installed and verified, and the rollback command is documented.
- The goal may implement and test code locally, but it must ask before installing the new Alleycat binary, restarting `alleycat.service`, stopping `codex-rc-keeper.service`, or otherwise changing the production runtime.
- Automated validation includes focused tests for the new crate, `cargo test -p alleycat-codex-remote-control` or the final crate package name, `cargo test -p alleycat`, rustfmt on touched Rust files, and `git diff --check`.
- After explicit restart/cutover approval, live validation checks `systemctl --user status alleycat.service`, `systemctl --user status codex-rc-keeper.service`, `alleycat status --json`, local listener/socket state, and journal evidence that native remote control returns to `connected` after at least one Alleycat restart.
- The goal commits locally if implementation work is performed, but it does not push `origin/main`; push remains a separate explicit decision after native soak.
