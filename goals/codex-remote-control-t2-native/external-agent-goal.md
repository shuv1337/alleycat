# External Goal: Codex Remote-Control T2 Cutover

You are running outside the Codex/ChatGPT session that depends on the current Alleycat service. Your job is to perform the production cutover for native Codex remote-control supervision from `/home/shuv/repos/alleycat`.

Do not push `origin/main`. Do not disable the T1 keeper until native T2 is installed, connected, and proven across an Alleycat restart.

## Source Of Truth

- Main goal: `goals/codex-remote-control-t2-native/goal.md`
- Cutover runbook: `goals/codex-remote-control-t2-native/external-cutover.md`
- Current handoff: `HANDOFF.md`
- Native implementation commit: `8ecddd8 feat: add native codex remote-control supervisor`
- External-runbook commit: `01e47fa docs: add external codex rc cutover runbook`
- Latest pending-cutover audit commit: `3df6c26 docs: record pending codex rc cutover audit`

## Required Workflow

1. Verify the repo is on the expected branch and includes the native commits:

   ```bash
   cd /home/shuv/repos/alleycat
   git status --short
   git log -3 --oneline
   ```

2. Run the non-production validation gates:

   ```bash
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
   cargo metadata --no-deps --format-version 1 >/tmp/alleycat-cargo-metadata.json
   git diff --check
   ```

3. Install and restart Alleycat:

   ```bash
   cargo install --locked --path crates/alleycat
   systemctl --user restart alleycat.service
   sleep 5
   systemctl --user status alleycat.service --no-pager
   systemctl --user status codex-rc-keeper.service --no-pager
   alleycat status --json
   ss -ltnp | rg '127\.0\.0\.1:(8390|8391|5852)'
   ```

4. Verify native T2 status:

   - `alleycat.service` is active.
   - `alleycat status --json` includes `codexRemoteControl`.
   - `codexRemoteControl.state` reaches `connected`.
   - `codexRemoteControl` contains no token material.
   - `codex-rc-keeper.service` remains active during this first native check.

5. Prove restart recovery:

   ```bash
   systemctl --user restart alleycat.service
   sleep 10
   alleycat status --json
   journalctl --user -u alleycat.service --since "10 minutes ago" --no-pager
   ```

   Required evidence:

   - Native `codexRemoteControl.state` returns to `connected`.
   - Journal evidence shows native supervisor initialization and enable/healthy behavior.
   - No `attestation/generate` blocker appears.
   - No token values appear in logs or status.

6. Disable T1 only after native restart recovery is proven:

   ```bash
   systemctl --user disable --now codex-rc-keeper.service
   systemctl --user status codex-rc-keeper.service --no-pager
   alleycat status --json
   ```

7. Record completion evidence in `HANDOFF.md`:

   - Exact install/restart time.
   - Alleycat service status.
   - Keeper status before and after disable.
   - `alleycat status --json` excerpt showing `codexRemoteControl.state == "connected"`.
   - Listener check for `8390`, `8391`, and `5852`.
   - Journal excerpt proving native reconnect after restart.
   - Whether rollback was needed.

8. Commit the evidence locally. Do not push.

## Rollback

If native T2 fails, leaks token material, records `blocked`, requests attestation, or destabilizes Alleycat:

```bash
systemctl --user enable --now codex-rc-keeper.service
systemctl --user status codex-rc-keeper.service --no-pager
```

If the native Alleycat binary must be backed out:

```bash
git revert 8ecddd8
cargo install --locked --path crates/alleycat
systemctl --user restart alleycat.service
systemctl --user enable --now codex-rc-keeper.service
```

Report exact failure evidence before taking broader action.
