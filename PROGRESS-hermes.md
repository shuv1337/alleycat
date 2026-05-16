# Hermes Plan Implementation Progress

Tracking in-flight work for `PLAN-improve-hermes-support.md`.

## Status — final

- [x] Phase 0 — gateway behavior characterized; gating table resolved in plan.
- [x] Phase 1 — `mode` config (`auto`/`api`/`cli`); health caching; parsed JSON; configurable timeout; mode honored by both `AgentManager` and bridge dispatch; `~/.hermes/.env` fallback for `API_SERVER_KEY`.
- [x] Phase 1.5 — Old per-turn pump deleted; trailing-chunk terminal handling moved into manager; dead `else if !auto_approve` branch removed; typed `TurnInterruptParams`.
- [x] Phase 2 — `RunStore` (`runs.json`) + `EventStore` (`events/<run_id>.jsonl`); persist `Starting` → `Running` → terminal; orphans marked `Unknown` at startup; atomic writes via tmp+rename.
- [x] Phase 3 — `HermesRunManager` owns single SSE consumer per `run_id`; `tokio::broadcast` to subscribers; events normalized via typed Codex notification structs and persisted before broadcast.
- [x] Phase 4 — Same-process reconnect: bridge-core ring + `EventStore` replay via `subscribe(run_id, after_seq)`. Daemon-restart with mid-flight runs: marked `Unknown` (gateway makes auto-reattach impossible). Fallback: `GET /v1/runs/{id}` polled when SSE pump dies before terminal event.
- [x] Phase 5.1 — `thread/read` / `thread/turns/list` reconstruct turns from `RunStore` + `EventStore`; integration test confirms cross-restart reconstruction. (Phase 5.2 dropped — no listing endpoint.)
- [x] Phase 6 — `hermes/approvalRequest` forwarded as server→client JSON-RPC request via `notifier().request(...)`; client choice (`once`/`session`/`always`/`deny`) parsed and POSTed back via `HermesApiClient::resolve_run_approval`; default-deny safety net on timeout / closed connection.
- [x] Phase 7 — CLI fallback structured logs (start, success, failure with elapsed_ms); CLI terminal persisted via `RunStore` for post-restart visibility.
- [x] Phase 8 — Structured logs on bridge construction, fallback selection, health probe failures, run pump start/end, SSE errors, status-poll fallback, approval forwarding.
- [x] Phase 9 — 19 tests pass: 13 unit (`RunStore`, `EventStore`, manager, SSE parser, API client) + 6 integration with fake gateway (happy path, failure, trailing-chunk terminal, approval bridge round-trip, post-restart `thread/read`, late subscriber `item/started` replay).
- [x] Phase 10 — `AGENTS.md` updated with Hermes section: config, auth, persistent state layout, gateway-side limits, reconnect/replay semantics, approval bridging, troubleshooting.

## Test summary

- `cargo test --workspace`: **649 passed**, 1 failed.
- The single failure (`crates/pi-bridge/tests/listen_mode_concurrent.rs::two_concurrent_connections_share_one_daemon`) is a **pre-existing pi-bridge integration test** that fails on clean `main` before any of these changes. Confirmed via `git stash && cargo test -p alleycat-pi-bridge --test listen_mode_concurrent` (still fails). Root cause: the spawned `alleycat-pi-bridge --listen` daemon completes `from_env().build()` (including a real-pi list_sessions RPC that takes >5s when no fake-pi script is bound right) but the socket never appears within the 5s `SPAWN_TIMEOUT`. Outside the scope of the Hermes plan.

## Files added

- `crates/hermes-bridge/src/run_state.rs` — `RunStore` / `HermesRunRecord` / `HermesTurnStatus`.
- `crates/hermes-bridge/src/event_store.rs` — `EventStore` / `NormalizedHermesEvent`.
- `crates/hermes-bridge/src/run_manager.rs` — `HermesRunManager` (single SSE owner + broadcast + status-poll fallback).
- `crates/hermes-bridge/src/health_cache.rs` — short-TTL `/health` snapshot cache.
- `crates/hermes-bridge/tests/integration_gateway.rs` — end-to-end tests with a hand-rolled fake gateway.

## Files modified

- `crates/alleycat/src/config.rs` — `HermesAgentConfig.{mode,health_timeout_ms,health_cache_ttl_ms}` + `HermesModeKind`.
- `crates/alleycat/src/agents.rs` — honor `mode`; better `hermes_api_available` with JSON parse + tracing.
- `crates/hermes-bridge/src/config.rs` — add `health_timeout_ms`, `health_cache_ttl_ms` with defaults.
- `crates/hermes-bridge/src/api_client.rs` — `ApprovalChoice` enum; `resolve_run_approval`; `get_run_status`; `RunStatusResponse`.
- `crates/hermes-bridge/src/bridge.rs` — wired `RunStore`/`EventStore`/`RunManager`/`HealthCache`; persisted `Starting`→`Running`→terminal; per-`Conn` translator subscribes to broadcast and bridges approval requests; CLI fallback logs + persistence; typed `turn/interrupt`; `~/.hermes/.env` API key fallback; `thread/read` reconstruction from persisted state.
- `crates/hermes-bridge/src/lib.rs` — export new modules + types.
- `crates/hermes-bridge/src/main.rs` — use `..Default::default()` for new fields.
- `AGENTS.md` — Hermes operational section.

## Operational constraints

- The user's running `alleycat.service` (PID 1812608) must not be killed without approval.
- All validation done via `cargo test`/`cargo build`; no `cargo install` / `systemctl restart`.
- Manual probe validation is only safe if it doesn't require a freshly installed binary; we lean on `cargo test --workspace` and `cargo run -p alleycat -- probe ...` against a *fake* gateway in tests.
