# External Cutover Instructions

These instructions are for an external agent or local terminal that is not currently depending on the Alleycat service being restarted. Do not run this cutover from a Codex/ChatGPT session that is connected through the Alleycat instance being restarted.

## Current Committed State

- Native T2 implementation commit: `8ecddd8 feat: add native codex remote-control supervisor`
- T2 crate: `crates/codex-remote-control/`
- Runtime owner: `alleycat.service`
- T1 rollback service: `codex-rc-keeper.service`
- Push policy: do not push `origin/main` until native T2 has soaked and the user explicitly approves the push.

## Preflight

Run these from `/home/shuv/repos/alleycat`:

```bash
git status --short
git log -1 --oneline
cargo test -p alleycat-codex-remote-control
cargo test -p alleycat
rustfmt --edition 2024 --check \
  crates/codex-remote-control/src/lib.rs \
  crates/codex-remote-control/src/auth.rs \
  crates/codex-remote-control/src/jsonrpc.rs \
  crates/codex-remote-control/src/status.rs \
  crates/codex-remote-control/src/supervisor.rs \
  crates/codex-remote-control/src/transport.rs \
  crates/alleycat/src/agents.rs \
  crates/alleycat/src/cli/status.rs \
  crates/alleycat/src/daemon/control.rs \
  crates/alleycat/src/daemon/mod.rs \
  crates/alleycat/src/http_server.rs \
  crates/codex-proto/src/notifications.rs
git diff --check
```

Expected:

- Latest implementation commit is present.
- Tests pass.
- Rustfmt check passes for touched Rust files.
- `git diff --check` is clean.
- `codex-rc-keeper.service` is still enabled/active before cutover.

## Install And First Restart

Do not stop T1 before native T2 is proven.

```bash
cargo install --locked --path crates/alleycat
systemctl --user restart alleycat.service
sleep 5
systemctl --user status alleycat.service --no-pager
systemctl --user status codex-rc-keeper.service --no-pager
alleycat status --json
```

Expected:

- `alleycat.service` is active.
- `codex-rc-keeper.service` is still active at this point.
- `alleycat status --json` includes `codexRemoteControl`.
- `codexRemoteControl.state` reaches `connected`.
- `codexRemoteControl` does not contain token material.

Check listeners:

```bash
ss -ltnp | rg '127\\.0\\.0\\.1:(8390|8391|5852)'
```

Expected listeners:

- `127.0.0.1:8390`
- `127.0.0.1:8391`
- `127.0.0.1:5852`

## Native Restart Recovery

After the first native status reaches `connected`, prove restart recovery:

```bash
systemctl --user restart alleycat.service
sleep 10
alleycat status --json
journalctl --user -u alleycat.service --since "10 minutes ago" --no-pager
```

Expected:

- `alleycat.service` is active after restart.
- `codexRemoteControl.state` returns to `connected`.
- Journal evidence shows the native supervisor initialized Codex app-server and called or maintained `remoteControl/enable`.
- No `attestation/generate` blocker appears.
- No token values appear in logs or status.

## Disable T1 Only After Native Is Proven

Only after native T2 reaches `connected` and survives at least one Alleycat restart:

```bash
systemctl --user disable --now codex-rc-keeper.service
systemctl --user status codex-rc-keeper.service --no-pager
alleycat status --json
```

Expected:

- `codex-rc-keeper.service` is disabled/inactive.
- Native `codexRemoteControl.state` remains `connected`.

## Rollback

If native T2 fails to connect, records `blocked`, requests attestation, leaks token material, or destabilizes Alleycat:

```bash
systemctl --user enable --now codex-rc-keeper.service
systemctl --user status codex-rc-keeper.service --no-pager
```

If the installed native Alleycat binary itself must be backed out, reinstall the prior known-good Alleycat binary or revert the native commit and reinstall:

```bash
git revert 8ecddd8
cargo install --locked --path crates/alleycat
systemctl --user restart alleycat.service
systemctl --user enable --now codex-rc-keeper.service
```

Do not push any rollback or native cutover follow-up without explicit user approval.

## Completion Evidence To Record

Record these in `HANDOFF.md` or a follow-up commit:

- Exact `cargo install` time.
- `systemctl --user status alleycat.service` result.
- `systemctl --user status codex-rc-keeper.service` result before and after T1 disable.
- `alleycat status --json` excerpt showing `codexRemoteControl.state == "connected"`.
- Listener check output for `8390`, `8391`, and `5852`.
- Journal excerpt showing native reconnect after the second Alleycat restart.
- Whether rollback was needed.
