# HANDOFF

## Objective

Keep the local codex app-server's ChatGPT remote-control connection alive so
`shuvdev` remains available as a remote-connected device without a manual
`codex remote-control` sidecar.

## Current status

T1 is implemented, installed, enabled, and active locally.

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

Do not push `origin/main` and do not start T2/native Rust integration until the
T1 service has stayed green for 24 hours and survived at least two alleycat
restart cycles.

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

- Observe the T1 service for 24 hours.
- Confirm it survives at least two alleycat restart cycles during that
  observation window.
- Only after that, decide whether to push `main` and begin T2/native alleycat
  integration.
