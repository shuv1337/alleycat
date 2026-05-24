# HANDOFF

## Objective

Keep the local codex app-server's ChatGPT remote-control connection alive so
`shuvdev` remains available as a remote-connected device without a manual
`codex remote-control` sidecar.

## Current status

T1 is implemented, installed, enabled, and active locally. T2 native code has
now been implemented in the checkout, but it has not been installed into the
production `~/.cargo/bin/alleycat` binary and `alleycat.service` has not been
restarted for cutover validation.

- Documentation handoff state was committed first in
  `f32c0c4 docs: record post-merge alleycat state and rc plan`.
- T1 implementation was committed in
  `8d2a951 feat: add codex remote-control keeper`.
- Repo source: `scripts/codex-rc-keeper`.
- Installed script: `~/.local/bin/codex-rc-keeper`.
- Repo unit template: `scripts/codex-rc-keeper.service`.
- Installed unit: `~/.config/systemd/user/codex-rc-keeper.service`.
- Service state: `codex-rc-keeper.service` is enabled and active.
- Alleycat state: `alleycat.service` is active with listeners on
  `127.0.0.1:8390`, `127.0.0.1:8391`, and `127.0.0.1:5852`.
- `~/.codex/log/codex-tui.log` was truncated before restart-heavy validation.
- T2 crate: `crates/codex-remote-control/`, package
  `alleycat-codex-remote-control`.
- External cutover runbook:
  `goals/codex-remote-control-t2-native/external-cutover.md`.
- T2 Alleycat wiring: `crates/alleycat/src/agents.rs` starts the native
  supervisor for `CodexMode::UnixProxy` after a reachable Unix app-server
  endpoint exists, stops it with the Codex app-server child, and exposes status
  through daemon status.
- Status surface: `alleycat status --json` includes `codexRemoteControl` in
  UnixProxy mode with state, server name, environment id, last update time,
  last enable reason, and redacted error/blocked summary.

Do not push `origin/main`. Do not install the new Alleycat binary, restart
`alleycat.service`, or stop/disable `codex-rc-keeper.service` without explicit
cutover approval.

## Implementation notes

- The codex app-server control socket speaks WebSocket-over-UDS with an HTTP
  Upgrade handshake. JSON-RPC messages are WebSocket text frames, not raw
  `Content-Length:` frames.
- The keeper sends `initialize`, then `initialized`, then consumes
  `remoteControl/status/changed`.
- It calls `remoteControl/enable` when status is disabled, errored, missing, or
  stale.
- The health loop runs about every 30s. Connected status is refreshed after
  about 300s to guard against stale snapshots.
- The keeper answers `account/chatgptAuthTokens/refresh` from
  `~/.codex/auth.json` without logging token values.
- The keeper does not opt into attestation. Any unexpected
  `attestation/generate` request is a blocker; inspect the journal and do not
  invent a token.

## Validation evidence

Commands already run successfully:

```bash
python -m py_compile scripts/codex-rc-keeper
scripts/codex-rc-keeper --once --dry-run-enable --post-enable-wait 1
cmp scripts/codex-rc-keeper ~/.local/bin/codex-rc-keeper
cmp scripts/codex-rc-keeper.service ~/.config/systemd/user/codex-rc-keeper.service
git diff --check
systemctl --user is-enabled codex-rc-keeper.service
systemctl --user is-active codex-rc-keeper.service
systemctl --user is-active alleycat.service
```

T2 local validation already run successfully:

```bash
cargo test -p alleycat-codex-remote-control
cargo test -p alleycat
rustfmt --edition 2024 --check <touched Rust files>
cargo metadata --no-deps --format-version 1
git diff --check
```

Non-destructive runtime audit on 2026-05-23 21:45:15 -0700:

- `systemctl --user is-active alleycat.service` => `active`.
- `systemctl --user is-active codex-rc-keeper.service` => `active`.
- `systemctl --user is-enabled codex-rc-keeper.service` => `enabled`.
- `alleycat status --json` did not include `codexRemoteControl`, so the native
  T2 binary has not been cut over yet.
- `~/.cargo/bin/alleycat` timestamp was `2026-05-22 22:50:00.898343723 -0700`,
  predating the native T2 commits.
- Listener check still showed `127.0.0.1:8390`, `127.0.0.1:8391`, and
  `127.0.0.1:5852` owned by the running `alleycat` process.

Important live evidence:

- One-shot probe reached `connected` over
  `~/.codex/app-server-control/app-server-control.sock` for `serverName=shuvdev`
  and `environmentId=env_e_6a0c127bd3a88333a18a070c79e945b3`.
- `alleycat.service` restart at 2026-05-23 09:39:13 closed the keeper socket.
- Keeper reconnected at 2026-05-23 09:39:29, saw `status=disabled`, called
  `remoteControl/enable`, then reached `status=connected` at
  2026-05-23 09:39:29.988.
- Later journal lines show periodic healthy status and successful stale-status
  refreshes.

## Useful commands

```bash
systemctl --user status codex-rc-keeper.service
journalctl --user -fu codex-rc-keeper.service
systemctl --user restart codex-rc-keeper.service
scripts/codex-rc-keeper --once --dry-run-enable
```

## Remaining work

- After explicit approval, run cutover: `cargo install --locked --path
  crates/alleycat`, `systemctl --user restart alleycat.service`, then verify
  `systemctl --user status alleycat.service`, `systemctl --user status
  codex-rc-keeper.service`, `alleycat status --json`, listener/socket state,
  and journal evidence that native remote control reaches `connected`.
- Because this Codex session is itself running behind Alleycat, do not perform
  the cutover from this session. Use
  `goals/codex-remote-control-t2-native/external-cutover.md` from an external
  terminal/agent.
- Restart Alleycat at least once more and verify native remote control returns
  to `connected`.
- Stop and disable `codex-rc-keeper.service` only after native restart recovery
  is proven.
- Commit locally after validation; do not push `origin/main` without a separate
  explicit decision.
