# Plan

## Solution Approach

Implement Codex remote-control supervision as a new workspace crate, `crates/codex-remote-control/`, with package name `alleycat-codex-remote-control`. The crate owns the WebSocket-over-Unix-socket JSON-RPC client, remote-control state machine, auth refresh handling, attestation blocker behavior, redacted status snapshots, and focused protocol tests.

Keep `crates/alleycat/src/agents.rs` thin: it starts or updates the native supervisor only after a Codex UnixProxy app-server endpoint is reachable, stops it when the Alleycat-owned Codex app-server child is stopped, and exposes the latest snapshot to daemon status. Do not re-enable or rely on `CodexMode::UnixDaemon`.

## Ordered Steps

1. Add the new workspace crate.

   Files and systems:
   - `Cargo.toml`: add `crates/codex-remote-control` as a workspace member and add `alleycat-codex-remote-control` under workspace dependencies.
   - `crates/codex-remote-control/Cargo.toml`: use workspace package metadata and dependencies on `anyhow`, `futures`, `serde`, `serde_json`, `thiserror`, `tokio`, `tokio-tungstenite`, `tracing`, and `alleycat-codex-proto`.
   - `crates/codex-remote-control/src/lib.rs`: export only the supervisor config, handle, snapshot, state, and error types Alleycat needs.

   Verification:
   - `cargo metadata --no-deps`
   - `cargo test -p alleycat-codex-remote-control`

2. Port the T1 keeper behavior into native Rust.

   Files and systems:
   - `scripts/codex-rc-keeper`: use as behavior reference only, not as runtime dependency.
   - `crates/codex-remote-control/src/transport.rs`: connect to the Codex control socket using `tokio::net::UnixStream` plus `tokio_tungstenite::client_async` with a stable local WebSocket URL such as `ws://codex-app-server.localhost/rpc`.
   - `crates/codex-remote-control/src/jsonrpc.rs`: implement minimal JSON-RPC request, notification, response, id tracking, timeout, and redacted error helpers.
   - `crates/codex-remote-control/src/auth.rs`: read `~/.codex/auth.json` or resolved `CODEX_HOME/auth.json`; return `accessToken`, `chatgptAccountId`, and `chatgptPlanType: null` without logging token values.
   - `crates/codex-remote-control/src/supervisor.rs`: perform `initialize`, send `initialized`, consume `remoteControl/status/changed`, call `remoteControl/enable` when status is missing, disabled, errored, or stale, reconnect on socket disconnect, and maintain the public status snapshot.
   - Reuse `alleycat-codex-proto` types for `remoteControl/status/changed` where practical; define small local request/response structs where `codex-proto` does not already cover the request.

   Verification:
   - Unit tests for status transitions: missing -> enable, disabled -> enable, errored -> enable, connected fresh -> no enable, connected stale -> enable.
   - Unit tests for auth refresh success and missing token/account failures using temp auth files.
   - Unit test that token values do not appear in `Debug`, status snapshots, or error summaries.
   - Integration-style test with a fake Unix-socket WebSocket server that observes `initialize`, `initialized`, `remoteControl/enable`, sends `remoteControl/status/changed`, and sends `account/chatgptAuthTokens/refresh`.
   - Test that `attestation/generate` records blocked/error status with method name and redacted params, without attempting to generate attestation.
   - `cargo test -p alleycat-codex-remote-control`

3. Wire the supervisor into Codex UnixProxy lifecycle.

   Files and systems:
   - `crates/alleycat/Cargo.toml`: depend on `alleycat-codex-remote-control`.
   - `crates/alleycat/src/agents.rs`: add a supervisor handle field to `AgentManager`.
   - `AgentManager::new`: construct the handle from the same resolved daemon launch environment used for Codex children, honoring `CODEX_HOME`.
   - `ensure_codex_unix_running`, `ensure_codex_unix_running_with_endpoint`, and `ensure_codex_unix_running_locked`: after every successful reachable endpoint path, start or update the supervisor with the concrete control socket path and auth path before returning. This must include the existing early return where a tracked child is alive.
   - `stop_codex_child` and `shutdown`: stop the supervisor when the managed Codex app-server child is stopped or Alleycat exits.
   - `restart_codex`: stop the supervisor before killing the child, then restart it after `ensure_codex_unix_running` succeeds.
   - Do not start the supervisor for `CodexMode::Stdio`, legacy `CodexMode::Websocket`, or `CodexMode::UnixDaemon`.

   Verification:
   - Focused tests or small helper tests for endpoint-to-socket-path selection, including default socket and Alleycat-owned alternate socket.
   - `rg "UnixDaemon|remote-control|codex_remote" crates/alleycat/src/agents.rs` review to confirm no `UnixDaemon` dependency was introduced.
   - `cargo test -p alleycat`

4. Add the status surface.

   Files and systems:
   - `crates/alleycat/src/daemon/control.rs`: extend `StatusInfo` with optional `codex_remote_control`.
   - `crates/alleycat/src/daemon/mod.rs`: include the supervisor snapshot in IPC `Status`.
   - `crates/alleycat/src/http_server.rs`: include the same snapshot in the PWA status path.
   - `crates/alleycat/src/cli/status.rs`: print a concise human line when the snapshot is present, while `--json` exposes state, server name, environment id, last update time, last enable reason, and error or blocked summary.

   Verification:
   - Serialization test for `StatusInfo` proving absent status remains backwards compatible.
   - Test or fixture asserting the JSON status object does not contain access tokens or token-like fields.
   - `alleycat status --json` after approved live cutover.

5. Document cutover and rollback.

   Files and systems:
   - `AGENTS.md`: update the T2 section after implementation to describe native supervision, status fields, keeper cutover, and rollback.
   - `HANDOFF.md` or the active plan/handoff document in repo root, if present: record exact validation state, what was installed, what remains soaking, and push policy.
   - Existing T1 assets remain in place during implementation: `scripts/codex-rc-keeper`, `scripts/codex-rc-keeper.service`, `~/.local/bin/codex-rc-keeper`, and `~/.config/systemd/user/codex-rc-keeper.service`.

   Verification:
   - Rollback commands are documented before disabling T1:
     - `systemctl --user enable --now codex-rc-keeper.service`
     - reinstall or restart the prior Alleycat binary if native supervision must be backed out.
   - No production runtime changes happen before explicit user approval.

6. Run local automated validation.

   Commands:
   - `cargo test -p alleycat-codex-remote-control`
   - `cargo test -p alleycat`
   - `cargo fmt --check` or rustfmt on all touched Rust files
   - `git diff --check`

   Stop here and ask before production cutover. Do not run `cargo install`, restart `alleycat.service`, stop `codex-rc-keeper.service`, or otherwise alter the live runtime without explicit approval.

7. After explicit cutover approval, install and verify live behavior.

   Commands and checks:
   - `cargo install --locked --path crates/alleycat`
   - `systemctl --user restart alleycat.service`
   - `systemctl --user status alleycat.service`
   - `systemctl --user status codex-rc-keeper.service`
   - `alleycat status --json`
   - Check listeners: `127.0.0.1:8390`, `127.0.0.1:8391`, and `127.0.0.1:5852`.
   - Check the Codex control socket and journal/log evidence that native remote control reaches `connected`.
   - Restart Alleycat at least once more and verify native remote control returns to `connected`.

   Cutover rule:
   - Stop and disable `codex-rc-keeper.service` only after native status is connected and restart recovery is proven.
   - Commit locally if implementation work was performed.
   - Do not push `origin/main`; push remains a separate explicit decision after soak.

## Risks and Open Questions

- The native WebSocket-over-UDS client should be validated against a fake server and then once against the live Codex control socket, because the T1 script manually implemented the HTTP Upgrade while Rust can use `tokio_tungstenite::client_async` over `UnixStream`.
- The supervisor start hook must cover every successful UnixProxy endpoint path, including early returns where the existing child is alive; otherwise remote control may silently not start after a normal steady-state connection.
- During implementation and pre-cutover testing, T1 may still enable remote control. Treat the native status as authoritative only after the approved cutover sequence disables T1.
- Auth refresh handling must preserve secret hygiene. Any logs, status snapshots, test failures, or errors that include token material are blockers.
- If Codex starts requiring attestation in this local path, T2 should stop with blocked/error evidence rather than faking support.
