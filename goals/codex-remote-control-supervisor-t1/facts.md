# Facts

- The current documentation handoff state is committed locally, including AGENTS.md and PLAN-codex-remote-control-supervisor.md, before or alongside the T1 implementation work.
- The T1 keeper is a standalone Python script installed at ~/.local/bin/codex-rc-keeper and uses only Python standard-library modules.
- The keeper connects to ~/.codex/app-server-control/app-server-control.sock as a WebSocket over the Unix socket, using standard-library HTTP Upgrade and WebSocket framing, and performs initialize followed by an initialized notification.
- The keeper tracks remote-control state from the initialized connection's remoteControl/status/changed notifications and from remoteControl/enable responses, and calls remoteControl/enable when the state is disabled, errored, missing, or stale.
- When app-server sends account/chatgptAuthTokens/refresh as a server-initiated request, the keeper answers with exact blocker evidence unless a safe token-refresh response shape is discovered from Codex Desktop or upstream tests during implementation.
- If remoteControl/enable requires an attestation handshake that is not already understood, the implementation stops and documents the blocker instead of inventing an unverified handshake.
- The keeper waits for the app-server socket on startup, reconnects after socket disconnects, logs to stdout or stderr for journald, and repeats status checks on a roughly 30-second loop.
- A user systemd unit exists at ~/.config/systemd/user/codex-rc-keeper.service, wants and starts after alleycat.service, restarts on failure, and runs ~/.local/bin/codex-rc-keeper.
- The codex-rc-keeper user service is daemon-reloaded, enabled, started, and observed as active or otherwise reported with exact blocker evidence.
- The huge ~/.codex/log/codex-tui.log is rotated or truncated before restart-heavy validation.
- Validation includes a live socket probe, keeper journal evidence, service status evidence, and at least one alleycat.service restart recovery check when safe.
- AGENTS.md documents the final T1 remote-control supervisor state and includes the 24-hour observation criterion for deciding whether to push main and begin T2.
- The implementation does not push origin/main and does not begin the native Alleycat T2 work in this goal.
