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
- OpenCode is lazily spawned by Alleycat with `opencode serve --port=<auto>` on first paired connection; local standalone `opencode serve` should not be left running unless intentionally testing outside Alleycat.
- The daemon spawns external CLIs on demand; availability is environment-dependent.

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
