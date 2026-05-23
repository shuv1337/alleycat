# Alleycat Agent Notes

## Project overview

Alleycat is a Rust workspace for an Iroh-backed daemon that multiplexes local coding-agent CLIs (Codex, Pi, Amp, OpenCode, Claude, Factory Droid, Hermes, Devin, and Grok) over a single QUIC connection for paired clients.

## Workspace layout

- `crates/alleycat`: main daemon/library and `alleycat` CLI binary.
- `crates/bridge-core`: shared JSON-RPC framing, launcher, session, pool, server, and state helpers.
- `crates/codex-proto`: Codex protocol models.
- `crates/{pi,amp,opencode,claude,droid,hermes,devin,grok}-bridge`: agent-specific bridge implementations.
- `crates/bridge-conformance`: conformance testing utilities.
- `scripts/`: helper scripts and shims.

## Build and install

- Build/check the workspace with `cargo build`, or test the daemon with `cargo test -p alleycat`.
- Install the source-built CLI globally with `cargo install --locked --path crates/alleycat`.
- The installed binary is expected at `~/.cargo/bin/alleycat`; ensure `~/.cargo/bin` is on `PATH` before reporting success.

## Operational notes

- `alleycat serve` is long-running; use an interactive/managed shell rather than plain `bash` if it must be run for validation.
- `alleycat install` configures per-user autostart (launchd/systemd user unit/Startup shortcut). Do not run it unless explicitly requested.
- On Linux, `alleycat install` writes/enables `~/.config/systemd/user/alleycat.service`; inspect with `systemctl --user status alleycat.service`.
- Runtime state and daemon logs live under `~/.local/state/alleycat/`; recent connection activity is in `~/.local/state/alleycat/logs/daemon.log.<date>`.
- Pairing payload/QR is printed with `alleycat pair --qr`; rotate exposed pairing tokens with `alleycat rotate`.
- Current upstream Codex handling prefers Codex's Unix app-server/proxy flow (`codex app-server --listen unix://` plus `codex app-server proxy`). The daemon also binds the configured loopback TCP `host`/`port` as a local websocket bridge for Codex Desktop clients, while older websocket-only Codex CLIs use that same address for the managed Codex child. A provider-router experiment listens on `port + 1` with paths like `/agent/claude` or `/agent/opencode`; it speaks Desktop websocket JSON-RPC and dispatches to the selected non-Codex Alleycat bridge.
- **PWA browser adapter is part of the production systemd unit.** The unit invokes:
  ```
  alleycat serve --serve-pwa --listen 127.0.0.1:5852 --pwa-dir /home/shuv/repos/litter-pwa/apps/web/dist
  ```
  Three listeners come up: `127.0.0.1:8390` (codex websocket bridge, from `host`/`port` in `~/.config/alleycat/host.toml`), `127.0.0.1:8391` (provider router), `127.0.0.1:5852` (PWA browser adapter). The PWA flag was originally opt-in for testing in the now-removed `alleycat-fork` sibling repo; that experiment has been merged into the single production unit on this fork. If `--serve-pwa` ever needs to be turned off, edit the unit with `systemctl --user edit --full alleycat.service`.
- OpenCode is lazily spawned by Alleycat with `opencode serve --port=<auto>` on first paired connection; local standalone `opencode serve` should not be left running unless intentionally testing outside Alleycat.
- The daemon spawns external CLIs on demand; availability is environment-dependent.

### Codex app-server lifecycle (orphan-daemon policy)

This fork **never** uses `CodexMode::UnixDaemon`, even when the local `codex` build supports `codex app-server daemon start`. Background:

- Upstream's `codex app-server daemon start` is the SSH/remote-deploy lifecycle manager. It double-forks and reparents the daemon to `systemd --user`, so the daemon outlives the process that started it.
- For a long-running local `alleycat serve`, that means the codex daemon survives `systemctl --user restart alleycat.service` and keeps whatever `cwd`/`env`/`PWD` it inherited at first launch. If the first launch happened while alleycat (or anything that shelled `codex`) was inside a project directory, every future chat opens in that project directory until the orphan is killed manually.
- `detect_codex_runtime()` (`crates/alleycat/src/agents.rs`) therefore skips the `UnixDaemon` branch and prefers `UnixProxy`. The retained `_codex_app_server_daemon_supported` probe is kept for documentation / future remote-bridge use and is `#[allow(dead_code)]`.
- `codex_command(bin)` pins every spawned `codex …` invocation to `current_dir($HOME)` as defense-in-depth. Any daemon that does escape (e.g. via an upstream bug) at least starts from `$HOME`, not from a stale project dir.
- Operational implication: there is **no codex app-server outside alleycat's cgroup** in steady state. If you see one, it is a regression — kill it, remove `~/.codex/app-server-daemon/app-server.pid` and `~/.codex/app-server-control/app-server-control.sock`, restart alleycat, and investigate the path that bypassed alleycat.
- The codex CLI wrapper at `~/dotfiles/codex/codex` and the Codex Desktop launcher (`~/repos/codex-desktop-linux/launcher/start.sh.template`) both refuse to start without a healthy alleycat, so the only legitimate path to a codex app-server child is through alleycat.

### Codex remote-control keeper

T1 remote-control supervision is installed as a standalone user service, not native Rust alleycat code yet.

- Repo source: `scripts/codex-rc-keeper`; installed script: `~/.local/bin/codex-rc-keeper`.
- Repo unit template: `scripts/codex-rc-keeper.service`; installed unit: `~/.config/systemd/user/codex-rc-keeper.service`.
- The unit is `Wants=alleycat.service`, `After=alleycat.service`, `Restart=on-failure`, and logs to journald through stdout/stderr.
- Transport is WebSocket-over-UDS to `~/.codex/app-server-control/app-server-control.sock` using an HTTP Upgrade handshake. JSON-RPC messages are WebSocket text frames, not raw `Content-Length:` frames.
- On each connection the keeper sends `initialize` and `initialized`, consumes `remoteControl/status/changed`, and calls `remoteControl/enable` when status is disabled, errored, missing, or stale. The default health loop is 30s; stale connected status is refreshed after 300s.
- On socket disconnect, usually from `systemctl --user restart alleycat.service`, the keeper reconnects to the respawned codex app-server and re-enables remote control from a fresh status snapshot.
- If app-server sends `account/chatgptAuthTokens/refresh`, the keeper answers from `~/.codex/auth.json` without logging token values. It does not opt into attestation; any unexpected `attestation/generate` request is a blocker and should be investigated from the keeper journal.

Useful commands:

```bash
systemctl --user status codex-rc-keeper.service
journalctl --user -fu codex-rc-keeper.service
systemctl --user restart codex-rc-keeper.service
scripts/codex-rc-keeper --once --dry-run-enable
```

Validation on 2026-05-23: the keeper was installed, enabled, and started; `~/.codex/log/codex-tui.log` was truncated before restart-heavy testing; an alleycat restart at 2026-05-23 09:39:13 was followed by keeper reconnect and `remoteControl/enable`, reaching `connected` at 2026-05-23 09:39:29.988. Do not push this branch or start T2/native Rust integration until the T1 service has stayed green for 24 hours and survived at least two alleycat restart cycles.

## Single-instance topology

As of 2026-05-22, this is the **only** alleycat instance on the machine. The former `~/repos/alleycat-fork` (dnakov upstream, browser-adapter experiment) has been deleted; its `--serve-pwa` browser-adapter feature was merged into this fork's production systemd unit. The sibling-isolation playbook (separate `HOME`, tmux session on `litter-pwa-alleycat.sock`, isolated XDG dirs under `/tmp/litter-pwa-alleycat-home`, port `5851`) is gone — none of those paths exist anymore.

Canonical facts:

| Fact | Value |
|---|---|
| Repo path | `/home/shuv/repos/alleycat` |
| Remote | `git@github.com:shuv1337/alleycat.git` (origin), `dnakov/alleycat` (upstream) |
| Binary in use | `~/.cargo/bin/alleycat` (release, `cargo install`ed from this tree) |
| Launch | systemd user unit `alleycat.service` |
| Listeners | `127.0.0.1:8390` (codex bridge), `127.0.0.1:8391` (provider router), `127.0.0.1:5852` (PWA browser adapter, `--pwa-dir /home/shuv/repos/litter-pwa/apps/web/dist`) |
| `HOME` / state | `~`, `~/.config/alleycat/`, `~/.local/state/alleycat/` |
| cgroup | `/user.slice/.../app.slice/alleycat.service` |

Operational rules carried over from the sibling era:

- `cargo install --locked --path crates/alleycat` from this tree overwrites `~/.cargo/bin/alleycat`, which is what `systemctl --user restart alleycat.service` re-execs. Never `cargo install` from an unrelated alleycat checkout into the same cargo bin without intending to swap the production binary.
- The QUIC/iroh relays emit a steady stream of `QADv6 NetworkUnreachable` IPv6 warnings into `~/.local/state/alleycat/logs/daemon.log.<date>`. Benign; ignore unless paired with actual connection failures.
- `pkill alleycat` is safe in this single-instance world but still inferior to `systemctl --user restart alleycat.service`, which preserves restart accounting and rebinds all three listeners cleanly.

## MCP servers on the codex bridge

As of 2026-05-22, **raindrop MCP has been removed** from `~/.codex/config.toml`, `~/.claude.json`, and the `pi-shuv` extensions list (`~/dotfiles/pi/pi-shuv/package.json`). Background: the codex app-server spawned by alleycat was leaking one `raindrop workshop mcp` child per chat session without reaping the previous ones, accumulating 4+ processes per hour and pushing alleycat's RSS to ~2.3 GB (6.6 GB peak) within a single uptime. If raindrop is re-enabled in any agent config, monitor `systemctl --user status alleycat.service` for child accumulation and consider a periodic restart timer.

The standalone `raindrop-workshop.service` user unit was also stopped and disabled (`systemctl --user disable raindrop-workshop.service`) on the same date — it is no longer expected to be running anywhere on this host. If a `raindrop workshop serve` process reappears under `systemd --user`, re-check whether the unit was re-enabled by some other workflow.

If you ever see `raindrop workshop mcp` after this cleanup, it is almost certainly a leftover child of a long-running standalone `codex resume` or `claude` CLI session that started before the config edits and is still holding its stale MCP wiring. It will die when its parent CLI exits, or you can kill it directly.

## Hermes bridge

- Configured under `[agents.hermes]` in the daemon TOML config:
  ```toml
  [agents.hermes]
  enabled = true
  mode = "auto"            # auto | api | cli
  bin = "hermes"            # CLI binary used in cli/auto fallback
  api_base = "http://127.0.0.1:8642"
  health_timeout_ms = 1000
  health_cache_ttl_ms = 2000
  ```
  - `auto` (default): probe `/health` and use the API path if healthy; fall back to the `hermes` CLI synthetic-completion path otherwise.
  - `api`: gateway only; gateway failure surfaces as `agent unavailable` and turns fail visibly (no silent CLI fallback).
  - `cli`: skip gateway probe entirely; every turn uses the CLI path.
- Auth: gateway expects `Authorization: Bearer <API_SERVER_KEY>` when an API key is configured. The bridge reads the key from `HERMES_API_KEY` or `API_SERVER_KEY` env vars, falling back to the `API_SERVER_KEY=...` line of `~/.hermes/.env`.
- Persistent state (per `codex_home`):
  - `<codex_home>/hermes-bridge/threads.json` — thread index (bridge thread id ↔ Hermes session id).
  - `<codex_home>/hermes-bridge/runs.json` — per-turn `HermesRunRecord` (status, run_id, accumulated text, agent item id, last event seq).
  - `<codex_home>/hermes-bridge/events/<run_id>.jsonl` — normalized bridge events appended per run; the only daemon-restart-survivable event log.
- Gateway-side limits (as of `hermes-agent` `gateway/platforms/api_server.py`):
  - `GET /v1/runs/{id}/events` is a **single-consumer** SSE stream; the queue is popped on consumer disconnect. The bridge therefore owns the only SSE consumer for a run's lifetime via `HermesRunManager` and broadcasts normalized events internally.
  - `GET /v1/runs/{id}` (status) persists for `_RUN_STATUS_TTL = 3600s` after a terminal state — the bridge polls this endpoint as a fallback when the SSE pump dies before a terminal event.
  - No `/v1/runs` listing endpoint; only Alleycat-created Hermes threads appear in `thread/list`.
  - Approval choices accepted by `POST /v1/runs/{id}/approval`: `once`, `session`, `always`, `deny` (plus `approve`/`approved`/`allow` aliases for `once`).
- Health-check command: `curl -fsS http://127.0.0.1:8642/health` (no auth).
- Manual smoke against a running daemon:
  ```bash
  alleycat probe --agent hermes --method thread/list --params '{}'
  alleycat probe --agent hermes --method thread/start \
    --params '{"cwd":"/tmp","model":"hermes-agent"}'
  alleycat probe --agent hermes --method turn/start \
    --params '{"threadId":"THREAD_ID","input":[{"type":"text","text":"hi"}]}' \
    --linger-secs 20
  ```
  - `alleycat status --json` shows agent availability (each `agents[].available` is computed by `AgentManager`; for Hermes that is gateway-health OR (in `auto` mode) `which hermes`).
- Reconnect / replay semantics:
  - Same-process, in-window: handled automatically by `bridge-core` session ring (`_alleycat_seq`).
  - Same-process, ring-evicted or new connection mid-run: `HermesRunManager::subscribe(run_id, after_seq)` replays from `events/<run_id>.jsonl` and then live-tails the broadcast.
  - Daemon restart with active runs: those runs are marked `Unknown` (the gateway's SSE queue is gone). Completed history is still reconstructed via `thread/read?include_turns=true` from `runs.json` + `events/`.
- Approval bridging: when a client connection is attached, the manager forwards `approval.request` as a server→client JSON-RPC request named `hermes/approvalRequest`. The client should reply with `{ "choice": "once|session|always|deny", "resolveAll": bool? }`. If no client answers within ~120s (default), the bridge posts `deny` to the gateway as a safety fallback.
- Troubleshooting:
  - `Invalid API key` from `POST /v1/runs`: missing or stale `API_SERVER_KEY`. Check `~/.hermes/.env`.
  - `404 run_not_found` on reconnecting to `/v1/runs/{id}/events`: expected (single-consumer); rely on `RunStore` + `EventStore` replay through Alleycat instead.
  - Empty `thread/list`: gateway has no listing endpoint; only Alleycat-created threads are tracked.
