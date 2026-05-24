# PLAN — Codex Remote Control Supervisor

**Goal:** Keep `shuvdev` continuously available as a ChatGPT remote-connected device with no manual sidecar, by supervising the outbound remote-control WebSocket on the alleycat-managed `codex app-server` child.

**Status:** T1 implemented, installed, and enabled locally. Under 24h observation before any push or T2 work.
**Owner:** local alleycat operator
**Last updated:** 2026-05-23

---

## Background

### Today's failure mode

- ChatGPT iOS/Mac/web apps connect to a local `codex app-server` via an **outbound WebSocket** to `wss://chatgpt.com/backend-api/wham/remote/control/server`. The server pushes "do work" frames; the local app-server streams events back.
- Enrollment ("hey relay, here's a new device called `shuvdev`") is **already done** and persisted in `~/.codex/state_5.sqlite.remote_control_enrollments` (one row, `server_name = shuvdev`, `account_id = 6b6ea04f-…`, `updated_at = 2026-05-19`). ChatGPT's relay still knows the device.
- The actual WebSocket session is started by calling `remoteControl/enable` on the local app-server's JSON-RPC bus. Once started, codex maintains it. But:
  - Nothing in alleycat or the codex wrapper calls `remoteControl/enable` after `alleycat.service` restarts.
  - If the WS dies mid-session (transient network blip, ChatGPT relay timeout, token rotation, etc.) and the app-server's reconnect logic gives up or doesn't exist, the device silently disappears from ChatGPT clients.
- The historical workaround — running `codex remote-control start` as a sidecar — conflicts with alleycat's daemon ownership ("`app server is running but is not managed by codex app-server daemon`") and was unstable.

### What's already in place

- The codex app-server JSON-RPC surface exposes:
  - `remoteControl/enable` — opens the outbound WS
  - `remoteControl/disable` — closes it
  - `remoteControl/status/changed` — reports current state as notifications
    to initialized clients. Newly initialized clients receive the current
    snapshot.
- The alleycat-managed app-server unix socket at
  `~/.codex/app-server-control/app-server-control.sock` accepts concurrent
  WebSocket clients over Unix sockets using a standard HTTP Upgrade handshake.
  JSON-RPC messages are WebSocket text frames, not raw LSP
  `Content-Length:` frames.
- Auth is already on file (`~/.codex/auth.json` has valid ChatGPT `id_token` / `access_token` / `refresh_token`, no API-key fallback).

### What's missing

A small process that:

1. Calls `remoteControl/enable` once after the codex app-server is up.
2. Tracks `remoteControl/status/changed` notifications and `enable`
   responses, and re-calls `enable` if the WS is not healthy or status goes
   stale.
3. Restarts cleanly across `alleycat.service` restarts.

---

## Two implementation tiers

### Tier 1 — Standalone script (today)

- Repo source: `scripts/codex-rc-keeper`; installed file:
  `~/.local/bin/codex-rc-keeper` (Python, stdlib only).
- Repo unit template: `scripts/codex-rc-keeper.service`; installed systemd
  user unit: `~/.config/systemd/user/codex-rc-keeper.service`
  - `After=alleycat.service`
  - `Wants=alleycat.service`
  - `Restart=on-failure`
  - `RestartSec=10s`
- Behavior:
  - On start: poll for the unix socket existing; wait up to 60s.
  - Open a WebSocket-over-UDS JSON-RPC client to `~/.codex/app-server-control/app-server-control.sock`.
  - Send `initialize` → expect response; send `initialized` notification.
  - Wait briefly for the initial `remoteControl/status/changed` snapshot. If
    the status is not `connected` or `connecting`, call `remoteControl/enable`.
  - Loop forever: every 30s, check cached status age + health and re-enable if
    needed.
  - If app-server sends `account/chatgptAuthTokens/refresh`, answer from
    `~/.codex/auth.json` using the current access token/account id without
    logging token values. If refresh requests repeat, surface the exact
    blocker instead of spinning noisily.
  - Do not opt into `capabilities.requestAttestation`. If app-server requires
    `attestation/generate` anyway, stop and document the exact request as a
    blocker.
  - On socket disconnect: log, sleep 5s, reconnect.
- Log to journald via stdout. Watch with
  `journalctl --user -fu codex-rc-keeper.service`.
- Success criteria: `shuvdev` stays visible in ChatGPT app for >24h across at least 2 `systemctl --user restart alleycat.service` cycles.

### Tier 2 — Native alleycat integration

- Implemented as a new crate: `crates/codex-remote-control/`, package `alleycat-codex-remote-control`.
- Lifecycle is wired from `crates/alleycat/src/agents.rs`: start after a reachable UnixProxy Codex app-server endpoint exists; stop with the managed Codex app-server child; restart when the child respawns.
- Same JSON-RPC client logic as T1 but in Rust, using a WebSocket client over
  Unix sockets.
- Status visible in `alleycat status --json` as `codexRemoteControl` with state,
  server name, environment id, last update time, last enable reason, and
  error/blocked summary. Token material must never appear there.
- Logs to `~/.local/state/alleycat/logs/daemon.log.<date>`.
- The standalone T1 unit remains installed until native cutover is explicitly
  approved, installed, restarted, and verified across at least one Alleycat
  restart.

---

## Risks / open questions

1. **Attestation handshake?** The codex protocol mentions
   `attestation/generate`. T1 intentionally does not opt into attestation; if
   app-server still requests it for remote-control, capture the exact request
   and stop rather than inventing a token.
2. **Token refresh.** ChatGPT auth tokens rotate. App-server may send a
   server-initiated `account/chatgptAuthTokens/refresh` request. T1 can answer
   with the current token/account id from `~/.codex/auth.json`, but repeated
   refresh requests mean the token is actually stale and should be reported as
   a blocker.
3. **Upstream churn.** `remote_control` feature flag is marked "removed" in `codex features list`. The JSON-RPC methods and sqlite table are still present in the current binary, but upstream may yank the whole flow. If T2 is shipped before that happens, we own the breakage. Mitigation: keep T1 in shape as a fallback, don't bet on T2 surviving major codex upgrades without re-validation.
4. **Single-client semantics.** Confirmed during T1: the unix socket accepts the
   keeper as an additional initialized WebSocket client without disrupting the
   existing alleycat/proxy session.
5. **`codex-tui.log` was huge.** It was truncated on 2026-05-23 before
   restart-heavy validation.

---

## Tasks (T1)

- [x] Rotate or truncate `~/.codex/log/codex-tui.log` (separate, do before next alleycat restart).
- [x] Write `scripts/codex-rc-keeper` and install it to `~/.local/bin/codex-rc-keeper` (Python, stdlib only).
- [x] Validate WebSocket-over-UDS handshake against the live socket (HTTP 101 -> initialize -> initialized -> initial status notification).
- [x] Confirm `remoteControl/enable` works without extra attestation.
- [x] Write `scripts/codex-rc-keeper.service` and install it to `~/.config/systemd/user/codex-rc-keeper.service`.
- [x] `systemctl --user daemon-reload && systemctl --user enable --now codex-rc-keeper.service`.
- [x] Restart alleycat and confirm the keeper reconnects and returns remote control to `connected`.
- [x] Document final state in `AGENTS.md` under "Codex remote-control keeper".
- [ ] Observe for 24h and at least two alleycat restart cycles before push or T2.

## Tasks (T2)

- [x] Decide: new `crates/codex-remote-control/` crate vs. inline module in `crates/alleycat/`.
- [x] Port the Python supervisor to Rust.
- [x] Wire lifecycle to the codex app-server child's start/stop in `agents.rs`.
- [x] Expose status in `alleycat status` JSON.
- [x] Validate locally with `cargo test -p alleycat-codex-remote-control` and `cargo test -p alleycat`.
- [ ] Run final `cargo fmt --check` and `git diff --check`.
- [ ] Install the new Alleycat binary after explicit approval.
- [ ] Restart Alleycat after explicit approval and verify native status reaches `connected`.
- [ ] Verify native restart recovery across at least one more Alleycat restart.
- [ ] Stop + disable the T1 systemd unit.
- [ ] Update `AGENTS.md`.

---

## Out of scope

- ChatGPT login / re-auth flows (already working).
- Claude / opencode / hermes remote control (existing crates already cover claude; others are TCP-direct).
- Codex Desktop local connectivity (already working via alleycat's unix proxy).
- Multi-account or multi-device support (single-device, single-account is the target).
