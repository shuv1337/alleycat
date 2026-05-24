# Codex Remote-Control Supervisor T1 Plan

## Solution Approach

Implement T1 as a small operational keeper with repo-tracked source plus installed user copies. The script should use Python stdlib only to open a WebSocket over `~/.codex/app-server-control/app-server-control.sock`, initialize as an app-server client, observe `remoteControl/status/changed`, call `remoteControl/enable` when needed, and run under a user systemd unit after `alleycat.service`.

The live transport was verified during planning: raw LSP framing on the Unix socket closes immediately, while HTTP Upgrade to WebSocket returns `HTTP/1.1 101 Switching Protocols`; after `initialized`, the app-server emits `remoteControl/status/changed` with `status: disabled` for `serverName: shuvdev`.

## Ordered Steps

1. Commit the existing handoff documentation state.

   Touches: `AGENTS.md`, `PLAN-codex-remote-control-supervisor.md`.

   Actions:
   - Review `git diff -- AGENTS.md` and `PLAN-codex-remote-control-supervisor.md`.
   - Commit those files locally with the documented post-merge/supervisor plan message.
   - Do not include the generated `goals/` package in that implementation commit unless explicitly desired at execution time.

   Verification:
   - `git status --short --branch`
   - `git show --stat --oneline HEAD`

2. Add a repo-tracked keeper source and service template.

   Touches: `scripts/codex-rc-keeper`, `scripts/codex-rc-keeper.service` or equivalent repo template path.

   Actions:
   - Implement WebSocket-over-UDS with stdlib modules: `socket`, `base64`, `hashlib`, `os`, `struct`, `json`, `time`, `select` or `selectors`, `argparse`, `logging`, and `pathlib`.
   - Provide a `--once` or `--probe` mode for validation and a default forever loop for systemd.
   - Implement client-masked WebSocket text frames, server frame reads, ping/pong or close handling if seen, and JSON-RPC request id tracking.
   - Send `initialize` with `clientInfo` and `capabilities.experimentalApi = true`, then send `initialized`.
   - Track current status from `remoteControl/status/changed` notifications and from `remoteControl/enable` responses.
   - Treat `connected` and short-lived `connecting` as healthy; call `remoteControl/enable` when status is `disabled`, `errored`, missing, or stale.
   - Handle server-initiated `account/chatgptAuthTokens/refresh` by reading `~/.codex/auth.json` and responding with `accessToken`, `chatgptAccountId`, and `chatgptPlanType: null`, without logging token values.
   - Do not opt into `capabilities.requestAttestation`. If app-server still requires `attestation/generate` for this flow, log the exact request and stop with a blocker.
   - Reconnect after socket disconnects and wait for the socket for up to 60 seconds on startup.

   Verification:
   - `python -m py_compile scripts/codex-rc-keeper`
   - `scripts/codex-rc-keeper --once --dry-run-enable` or the implemented non-mutating probe option should show WebSocket initialize and the latest status notification.
   - If the first mutating validation is safe, `scripts/codex-rc-keeper --once` should call `remoteControl/enable` only when the status is not healthy and print redacted status evidence.

3. Rotate the huge Codex TUI log before restart-heavy validation.

   Touches: `~/.codex/log/codex-tui.log`.

   Actions:
   - Move or truncate the current `~/.codex/log/codex-tui.log`, which is about 106 GiB in the planning snapshot.
   - Prefer preserving a timestamped zero-cost marker or rotated path if disk allows; otherwise truncate in place.

   Verification:
   - `du -h ~/.codex/log/codex-tui.log`
   - `ls -lh ~/.codex/log/codex-tui.log*`

4. Install the keeper and user service.

   Touches: `~/.local/bin/codex-rc-keeper`, `~/.config/systemd/user/codex-rc-keeper.service`.

   Actions:
   - Install the repo script to `~/.local/bin/codex-rc-keeper` and make it executable.
   - Install the service unit with `Wants=alleycat.service`, `After=alleycat.service`, `Restart=on-failure`, `RestartSec=10s`, and `ExecStart=/home/shuv/.local/bin/codex-rc-keeper`.
   - Run `systemctl --user daemon-reload`.
   - Run `systemctl --user enable --now codex-rc-keeper.service`.

   Verification:
   - `cmp scripts/codex-rc-keeper ~/.local/bin/codex-rc-keeper`
   - `systemctl --user cat codex-rc-keeper.service`
   - `systemctl --user is-enabled codex-rc-keeper.service`
   - `systemctl --user status codex-rc-keeper.service --no-pager`

5. Validate live behavior and restart recovery.

   Touches: live `alleycat.service`, live Codex app-server socket, keeper journal.

   Actions:
   - Confirm `alleycat.service` is active and the Codex app-server control socket is listening.
   - Watch keeper logs for initialize, current status, enable attempt/result, and steady polling.
   - Restart `alleycat.service` once when safe and confirm the keeper reconnects and re-enables or observes healthy status.
   - If remote-control stays disabled or errored, capture exact JSON-RPC error, status notification, and journal lines.

   Verification:
   - `systemctl --user status alleycat.service --no-pager`
   - `ss -xlpn | rg 'app-server-control.sock|codex'`
   - `journalctl --user -u codex-rc-keeper.service -n 100 --no-pager`
   - `systemctl --user restart alleycat.service`
   - `journalctl --user -u codex-rc-keeper.service -f` during the restart window, or a bounded `-n` check after.

6. Document final operational state.

   Touches: `AGENTS.md`, optionally `PLAN-codex-remote-control-supervisor.md`.

   Actions:
   - Add an `AGENTS.md` section for the T1 remote-control supervisor: script path, service path, log command, restart behavior, expected socket transport, and attestation/auth caveats.
   - Record that T2/native Alleycat work should wait until the keeper has stayed green for 24 hours and survived at least two `alleycat.service` restarts.
   - Keep origin push deferred.

   Verification:
   - `rg -n 'Remote control supervisor|codex-rc-keeper|24' AGENTS.md PLAN-codex-remote-control-supervisor.md`
   - `git diff --check`
   - `git status --short --branch`

7. Commit local T1 work without pushing.

   Touches: git only.

   Actions:
   - Commit repo-tracked keeper source/template and documentation updates.
   - Leave `origin/main` unpushed until the 24-hour observation window passes.

   Verification:
   - `git log --oneline --decorate -5`
   - `git status --short --branch` should show a clean repo except any intentionally untracked goal files.
   - Do not run `git push`.

## Risks And Open Questions

- `remoteControl/status/read` appears stale. Current evidence points to `remoteControl/status/changed` as the status source and `remoteControl/enable` as the active probe/mutator.
- WebSocket-over-UDS is more code than raw LSP framing. Keep it small, covered by `--once` validation, and avoid a dependency because the accepted T1 constraint is stdlib only.
- Token refresh response shape is known from upstream protocol tests, but returning the current `~/.codex/auth.json` token may not solve a genuinely expired-token case. If refresh requests repeat, document the exact failure and do not loop noisily.
- Attestation is intentionally not opted into. If the remote-control path requires `attestation/generate`, stop with blocker evidence.
- The ChatGPT app visibility check is partly manual. Terminal validation can prove the local WebSocket, enable response, service status, and restart recovery; actual device visibility may need user confirmation in ChatGPT iOS/Mac/web.
