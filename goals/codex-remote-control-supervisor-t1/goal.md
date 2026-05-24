# Codex Remote-Control Supervisor T1

Implement the standalone T1 codex remote-control keeper for Alleycat so the local Codex app-server keeps `shuvdev` visible as a ChatGPT remote-connected device without a manual sidecar. The goal includes committing the current documentation handoff state, installing and enabling the user-level keeper service, validating live recovery, and documenting the final operational state.

Use `facts.md` as the accepted shared understanding for required behavior and verification expectations.

Use `plan.md` as the execution plan. It includes the corrected WebSocket-over-UDS transport path, status-notification based health tracking, systemd installation, live validation, and the explicit no-push/no-T2 boundary.

Done means the accepted facts are satisfied or any remaining blocker is documented with exact command output, JSON-RPC error/status evidence, and systemd/journal evidence. `origin/main` must not be pushed and native Alleycat T2 work must not begin in this goal.
