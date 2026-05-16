# Plan: Improve Hermes Support in Alleycat

## Objective

Make Alleycat's Hermes backend feel like a first-class, serverful agent backend comparable to OpenCode/Codex: reliable gateway detection, durable thread/session mapping, live streaming through Hermes SSE, reconnect/replay support for in-flight turns, and clear fallback behavior when the Hermes gateway is unavailable.

The immediate motivation is that Hermes already has a long-running gateway (`http://127.0.0.1:8642`) with REST + SSE primitives, but Alleycat's current `hermes-bridge` is a thin first pass. It can start a bridge-created thread and stream a run while the initiating connection is alive, but it does not yet provide strong mid-flight reattach/replay semantics across daemon restarts or ring-eviction, robust health/status reporting, gateway session import, or parity with OpenCode's more durable SSE event consumer model.

## What Alleycat Already Gives Us (do not re-implement)

Before scoping new work, anchor on the reconnect machinery that already exists in `crates/bridge-core/src/session/`:

- Every `ctx.notifier().send_notification(...)` call in `hermes-bridge/src/bridge.rs` routes through `Session::enqueue`, which:
  - stamps a top-level `_alleycat_seq` on JSON-object payloads,
  - appends to a per-session `ReplayRing` (size-bounded msgs + bytes),
  - forwards to the live drainer if attached.
- When the iroh stream closes, `serve_stream_with_session` calls `session.drop_attachment()` — but **the spawned SSE pump task keeps running and keeps enqueueing** into the ring.
- A reconnect that calls `install_attachment(Some(last_seen))` automatically replays the in-ring backlog and re-emits any outstanding server→client requests via `serverRequest/replay`.

Practical consequence: **same-process, in-window mid-flight reconnect already works** for Hermes notifications today. The new work in this plan targets the cases where the ring is not enough:

1. **Ring drift** — long disconnects, or noisy sessions that evict frames past the cursor.
2. **Daemon restart** — the ring is in-memory only; nothing survives an Alleycat restart.
3. **Multi-subscriber fan-out** — `Session` is single-attachment; we have no way to mirror an active run to a second connection.
4. **Pump death** — the spawned SSE task is bound to a single `Conn` clone; if the underlying SSE socket errors out, the run is over even if the gateway run is still alive.

Frame all durability/reattach phases below in those terms.

## Current State

### Local runtime verified

Current local Alleycat config:

```toml
[agents.hermes]
enabled = true
bin = "hermes"
api_base = "http://127.0.0.1:8642"
```

Verified gateway health:

```text
GET http://127.0.0.1:8642/health    -> {"status":"ok","platform":"hermes-agent"}
GET http://127.0.0.1:8642/v1/health -> {"status":"ok","platform":"hermes-agent"}
```

Verified bridge handshake:

```bash
alleycat probe --agent hermes --method thread/list --params '{}'
```

Returned a valid bridge response with an empty thread list.

### Relevant code

- `crates/alleycat/src/agents.rs`
  - Constructs `HermesBridge` in `AgentManager::new` with `state_dir = codex_home.join("hermes-bridge")`.
  - **Always** sets `HermesMode::Auto { api_base, bin }` from `HostConfig` — the `Api` / `Cli` arms are not currently selectable from user config.
  - Computes Hermes availability via `hermes_available()` and `hermes_api_available()` (300ms timeout, status-code only, no JSON parse, no caching).
- `crates/alleycat/src/config.rs`
  - Host config shape for `[agents.hermes]` — exposes `enabled`, `bin`, `api_base`; no `mode` field.
- `crates/hermes-bridge/src/lib.rs`
  - Public bridge module and mode summary.
- `crates/hermes-bridge/src/config.rs`
  - Defines `HermesMode::{Api,Cli,Auto}` and `HermesBridgeConfig` (already accepts `state_dir`).
- `crates/hermes-bridge/src/api_client.rs`
  - Implements Hermes gateway REST/SSE client:
    - `GET /health`
    - `POST /v1/runs`
    - `GET /v1/runs/{run_id}/events`
    - `POST /v1/runs/{run_id}/stop`
    - `POST /v1/runs/{run_id}/approval` (hard-coded `{"choice":"once"}`)
- `crates/hermes-bridge/src/bridge.rs`
  - Implements Codex-compatible JSON-RPC surface for Hermes.
  - Currently creates local bridge thread bindings.
  - On `turn/start`, creates a Hermes run and spawns a per-turn SSE pump task.
  - Has **5+ duplicated exit paths** in `pump_api_events`, each manually calling `emit_agent_completed` + `emit_turn_completed`.
  - Contains a dead `else if !auto_approve` branch inside an `else` that already excluded `auto_approve == true`.
  - Post-loop body flush only handles `message.delta`, not terminal events — a terminal event in the trailing un-`\n\n`-terminated chunk would be missed.
  - `turn/interrupt` reads `params["threadId"]` directly instead of using a typed param struct.
  - Re-calls `api_client.health()` on **every** `turn/start` in `Auto` mode (in addition to `list_agents` calls).
- `crates/hermes-bridge/src/index.rs`
  - Stores Hermes thread/session bindings (`threads.json` under `state_dir`).
- `crates/hermes-bridge/src/state.rs`
  - Tracks active turns in-memory (`ActiveTurn`); no persistence.
- `crates/hermes-bridge/src/sse.rs`
  - Parses Hermes SSE frames.
- `crates/hermes-bridge/src/cli_adapter.rs`
  - CLI fallback path — non-streaming; synthesizes a single completion message.
- `crates/opencode-bridge/src/handlers/mod.rs`
  - Useful reference: long-lived SSE consumer and event pump pattern.
- `crates/opencode-bridge/src/opencode_proc.rs`
  - Useful reference: managed/external runtime handling and readiness checks.
- `crates/bridge-core/src/session/*`
  - **Already-built** session replay/ring/outstanding-request infrastructure described above.
- `crates/bridge-core/src/server.rs` / `crates/bridge-core/src/envelope.rs`
  - Bridge stream serving and JSON-RPC envelope handling. `serve_stream_with_session` already supports `last_seen` cursor and `drop_attachment` on disconnect.
- `crates/pi-bridge/src/approval.rs`, `crates/pi-bridge/src/handlers/turn.rs`
  - Reference for full server→client request bridging using `notifier().request(...)` and the session pending/outstanding tables.

### Seq taxonomy (don't conflate)

Two independent monotonic counters will exist:

- **`_alleycat_seq`** — minted by `Session::enqueue`; per-session; stamped on outbound JSON-RPC frames; consumed by bridge-core's replay ring. Drives same-process reconnect.
- **`event.seq`** — minted by `HermesRunManager` on each normalized Hermes event; per-`run_id`; persisted to `events/<run_id>.jsonl`. Drives daemon-restart-survivable replay.

After a daemon restart, replaying persisted normalized events through `ctx.notifier()` re-stamps them with new `_alleycat_seq` values — that is correct and expected.

## Target Behavior

### User-facing goals

- Hermes appears as available when either:
  - the configured gateway is healthy, or
  - CLI fallback is usable (in `Auto` mode only).
- Hermes `thread/start` + `turn/start` streams reliably through the Litter/Alleycat client.
- If the client disconnects during a Hermes run, reconnecting to the same Alleycat session should recover useful state:
  - active turn metadata,
  - already-emitted items/deltas where possible,
  - subsequent SSE events from the active run where Hermes supports continued event streaming.
- If a Hermes gateway run is still active, Alleycat should be able to reattach an event pump by `run_id` rather than losing the stream permanently.
- Hermes bridge-created threads should survive daemon restarts via persistent index/state.
- Failure modes should be explicit in the app instead of silently falling back or hanging.

### Non-goals for the first implementation slice

- Full import of every historical Hermes session if the gateway does not expose a stable listing API.
- Perfect attach to runs created outside Alleycat unless Hermes exposes enough API to enumerate and correlate active runs.
- Full parity for unsupported Codex APIs such as review mode, rollback semantics, or streaming PTY support.
- Feature parity for the CLI fallback — it remains a non-streaming, single-completion path.

## Design Principles

1. **Gateway-first, CLI-fallback explicitness**
   - Prefer Hermes gateway when healthy.
   - Fall back to CLI only in `Auto` mode and only with clear telemetry/logging.
   - In `Api` mode, gateway failure should be a visible error, not silent CLI fallback.

2. **Durable state before streaming**
   - Persist thread/turn/run metadata before returning `turn/start` success.
   - A reconnect should not depend on in-memory-only `ActiveTurn` state.

3. **Connection pumps are not ownership**
   - The Hermes run belongs to the gateway.
   - Alleycat bridge state should know how to reattach to it.
   - Per-connection event pumps should subscribe to a durable run/event manager rather than owning the only SSE reader when possible.

4. **Telemetry is definition-of-done**
   - Add structured logs for health checks, run creation, SSE connect/disconnect, reconnect, terminal events, errors, fallback decisions, and latency.
   - Include stable identifiers: `thread_id`, `turn_id`, `hermes_session_id`, `run_id`, `api_base`, `agent=hermes`.

## Proposed Architecture

### Current model

```text
Litter client
  -> Iroh stream
    -> Alleycat serve_stream_with_session  (ring + drainer + outstanding-request replay)
      -> HermesBridge::dispatch(turn/start)
        -> POST /v1/runs
        -> spawn per-turn task (owns Conn clone, owns SSE reader)
          -> GET /v1/runs/{run_id}/events
          -> ctx.notifier().send_notification(...) -> session ring (per-session _alleycat_seq)
```

### Target model

```text
Litter client(s)
  -> Iroh stream(s)
    -> Alleycat serve_stream_with_session  (unchanged; still owns ring + drainer)
      -> HermesBridge
        -> HermesRunManager
          -> RunStore       (runs.json)
          -> EventStore     (events/<run_id>.jsonl, per-run event.seq)
          -> active SSE task per run_id  (one, not per-connection)
          -> broadcast::Sender<NormalizedHermesEvent>
          -> per-connection subscribers translate -> ctx.notifier().send_notification(...)
        -> Hermes gateway HTTP/SSE
```

`HermesRunManager` owns SSE ownership and event durability. Connections subscribe to run events; they do not own the sole source of truth.

## Data Model Changes

### Existing `HermesBinding`

`HermesBinding` currently maps bridge thread IDs to Hermes sessions. Leave its on-disk schema alone in the first cut; complement it with **additive** files under the existing `state_dir = <codex_home>/hermes-bridge/`.

Files added by this plan:

```text
<state_dir>/threads.json             # existing
<state_dir>/runs.json                # new — RunStore
<state_dir>/events/<run_id>.jsonl    # new — EventStore
```

### New `HermesRunRecord`

Defined in `crates/hermes-bridge/src/run_state.rs`:

```rust
pub struct HermesRunRecord {
    pub thread_id: String,
    pub turn_id: String,
    pub hermes_session_id: String,
    pub run_id: Option<String>,
    pub status: HermesTurnStatus,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    pub error: Option<String>,
    pub accumulated_text: String,
    pub last_event_seq: Option<u64>,
}

pub enum HermesTurnStatus {
    Starting,
    Running,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}
```

### Event log format

Persist normalized bridge events (not raw Hermes events) keyed by per-run `event.seq`:

```json
{
  "seq": 1,
  "ts": 1778900000000,
  "thread_id": "thread_...",
  "turn_id": "turn_...",
  "run_id": "run_...",
  "method": "agentMessage/delta",
  "params": { ... }
}
```

Benefits:

- Easy replay to reconnecting clients (re-emit via `ctx.notifier()` — bridge-core re-stamps `_alleycat_seq`).
- Decouples replay from Hermes event schema drift.
- Daemon-restart-survivable.

## Implementation Plan

## Phase 0 — Baseline Verification and Characterization (gate)

**Completed** (`hermes-agent` source at `~/repos/hermes-agent/gateway/platforms/api_server.py`, gateway running at `http://127.0.0.1:8642`). Findings drive Phase 3/4/5/6 design.

### Confirmed gateway behavior

- **Auth**: Bearer token in `Authorization` header. Source: `API_SERVER_KEY` env var (or `extra.key` in `platforms.api_server` config). Stored in `~/.hermes/.env` as `API_SERVER_KEY=hgw_...`. When empty/unset, all requests are allowed (loopback-only fallback).
- **`POST /v1/runs`** returns `{"run_id": "run_<hex>", "status": "started"}`. No `session_id` returned — gateway internally uses `session_id = body.session_id or run_id`. Body accepts: `input` (string or messages array), `session_id`, `previous_response_id`, `conversation_history`, `instructions`, `model`. Concurrency cap: 10 concurrent runs.
- **`GET /v1/runs/{run_id}/events`** is **single-consumer SSE, not reopenable**. Implementation pops `run_streams[run_id]` in `finally:` when the consumer disconnects. After the run terminates the queue is also popped, and subsequent subscribers get `404 run_not_found`. There is a tiny race window where a quick reconnect *might* hit the same queue before the prior consumer's `finally` runs, but it is not a designable guarantee.
- **Events carry no stable IDs**, no seq numbers, no replay cursor. Only a float `timestamp`. Event shapes observed:
  - `{event: "message.delta", run_id, timestamp, delta}`
  - `{event: "reasoning.available", run_id, timestamp, text}`
  - `{event: "tool.started", run_id, timestamp, tool, preview}`
  - `{event: "tool.completed", run_id, timestamp, tool, duration, error}`
  - `{event: "approval.request", run_id, timestamp, choices: ["once","session","always","deny"], ...}`
  - `{event: "approval.responded", run_id, timestamp, choice, resolved}`
  - `{event: "run.completed", run_id, timestamp, output, usage: {input_tokens, output_tokens, total_tokens}}`
  - `{event: "run.failed"|"run.cancelled", run_id, timestamp, message|error}` (per `api_client.rs`)
- **`GET /v1/runs/{run_id}`** returns pollable run status as `{object: "hermes.run", run_id, status, updated_at, created_at, session_id, model, last_event, output, usage}`. **Persists for `_RUN_STATUS_TTL = 3600` seconds after terminal state**, so we can recover terminal output even after the SSE queue is gone.
- **`POST /v1/runs/{run_id}/approval`** accepts `{choice: "once"|"session"|"always"|"deny", all?: bool, resolve_all?: bool}`. Aliases: `approve`/`approved`/`allow` → `once`. Returns 409 `approval_not_pending` if no pending approval; 409 `approval_not_active` if run has no approval session; 200 `{run_id, choice, resolved: N}` on success. Emits `approval.responded` event on the SSE stream.
- **`POST /v1/runs/{run_id}/stop`** marks status `stopping`; calls `agent.interrupt(...)` and cancels the task with a 5s wait cap.
- **No `/v1/runs` listing endpoint.** No `/v1/sessions`. No `/openapi.json`. Only `GET /v1/runs/{id}` for known ids.
- **Health**: `GET /health` and `GET /v1/health` both return `{status: "ok", platform: "hermes-agent"}` and do not require auth.

### Gating decisions (resolved)

| Unknown | Verdict | Plan impact |
|---|---|---|
| `/v1/runs/{id}/events` reopenable after disconnect | **No, not reliably.** Queue is popped on any consumer disconnect or run termination. | `HermesRunManager` must own the **single** SSE consumer for a run's lifetime. Pump death = run lost on the SSE side; recovery falls back to polling `GET /v1/runs/{id}` for terminal status + `output`. New connections subscribe to a broadcast, never open a second SSE stream. |
| Events carry stable IDs / replay cursor | **No.** Only float `timestamp`. | Dedupe is Alleycat-side only. `event.seq` is minted by `EventStore::append`. Replay from `EventStore` is the only source of truth for missed events. |
| Session / run listing exists | **No.** | Phase 5.2 **dropped**. Document explicitly: only Alleycat-created threads appear in `thread/list`. |
| `POST /v1/runs/{id}/approval` supports `deny` / `always` / `session` | **Yes** (full set: `once`, `session`, `always`, `deny`, plus `approve`/`approved`/`allow` aliases for `once`). | Phase 6 wires the full decision matrix. |
| Terminal status recoverable after stream death | **Yes**, for up to 3600s via `GET /v1/runs/{id}`. | Manager polls status endpoint as fallback when SSE pump dies before terminal event. |

### Bootstrap requirements baked into the plan

- `HermesBridge` must read `API_SERVER_KEY` from env (already wired in `api_client.rs::DEFAULT_API_KEY_ENV`). Add fallback: if env unset, attempt to read `~/.hermes/.env` line `API_SERVER_KEY=...`. (Optional; nice-to-have. Env-var path is sufficient for systemd unit.)
- Add `GET /v1/runs/{run_id}` to `HermesApiClient` (`get_run_status`) — Phase 3/4 needs it for the terminal-status fallback.
- Run-create response struct already tolerant of missing `session_id` (`#[serde(default)]`).

## Phase 1 — Availability, Diagnostics, and Mode Configuration

This phase combines what was previously Phases 1 and 7's config work, because Phase 1's validation criteria ("`Api` mode errors visibly") are not testable without user-selectable `mode`.

### 1.1 Expose `mode` in host config

`HermesMode::{Api,Cli,Auto}` exists in `crates/hermes-bridge/src/config.rs` but `agents.rs` always constructs `Auto`. Make it user-selectable:

- [ ] Add to `[agents.hermes]` in `crates/alleycat/src/config.rs`:
  ```toml
  [agents.hermes]
  enabled = true
  mode = "auto"   # auto | api | cli
  bin = "hermes"
  api_base = "http://127.0.0.1:8642"
  ```
- [ ] Map host config `mode` to `HermesMode` enum in `agents.rs`.
- [ ] Default to `auto` for backward compatibility.

### 1.2 Strengthen gateway health detection

Current `hermes_api_available()` only checks for HTTP success with a 300ms timeout. Health endpoint does **not** require auth (Phase 0).

- [ ] Update `crates/alleycat/src/agents.rs::hermes_api_available` to parse health response and require `status == "ok"` when JSON is present.
- [ ] Raise timeout to ~1s; make configurable via `[agents.hermes].health_timeout_ms`.
- [ ] Include configured `api_base` and failure reason in structured logs.
- [ ] In `Api` mode: gateway failure surfaces as `agent unavailable` with reason; never silently falls back to CLI.
- [ ] In `Cli` mode: skip the health probe entirely.

### 1.3 Share health caching between `list_agents` and `dispatch_turn_api`

Today both `AgentManager::hermes_available` and `HermesBridge::handle_turn_start` hit the gateway. The second call is on the per-turn hot path.

- [ ] Move cached availability to a shared `Arc<HealthCache>` owned by `HermesBridge` (or wired through `AgentManager`), so both call sites read the same value.
- [ ] TTL ~2s; explicit refresh on probe error.

```rust
struct CachedAvailability {
    checked_at: Instant,
    available: bool,
    reason: Option<String>,
}
```

### 1.4 Expose mode/status in agent metadata if possible

- [ ] Consider adding debug-only or status JSON details indicating Hermes mode:
  - `apiHealthy: true/false`
  - `apiBase: http://127.0.0.1:8642`
  - `fallbackAvailable: true/false`
- [ ] If protocol changes are too risky, log this in daemon logs only.

Validation:

- [ ] With gateway running, Hermes available; one health probe per ~2s, not per turn.
- [ ] With gateway stopped, `mode = "auto"` + CLI present: Hermes still available with explicit fallback log.
- [ ] With gateway stopped, `mode = "api"`: Hermes unavailable; `turn/start` returns a visible error, no CLI invocation.
- [ ] With `mode = "cli"`: no gateway probe occurs for turn execution.

## Phase 1.5 — Pump Refactor (tracer-bullet prerequisite)

The current `pump_api_events` is a correctness landmine for any new state-writing work because of its duplicated exit paths and dead branch. Fix it before Phase 2 writes anything to `RunStore`.

- [ ] Extract a single `finalize_turn(ctx, thread_id, turn_id, agent_item_id, full_text, outcome)` helper.
  - `outcome: enum { Success, Failed(String), Cancelled }`.
  - Emits `item/completed` for agent message, then `turn/completed` with correct `TurnStatus`.
  - Single place where future Phase 2 code will `run_store.mark_terminal(...)`.
- [ ] Replace all 5+ exit paths in `pump_api_events` with `finalize_turn` calls.
- [ ] Remove the dead `else if !auto_approve` branch; restructure approval handling as `match (auto_approve, event)`.
- [ ] Fix trailing-chunk handling: after the read loop, drain `body` for terminal events (not just deltas) before defaulting to success.
- [ ] Convert `handle_turn_interrupt` to use typed `TurnInterruptParams`.
- [ ] Add unit tests for `finalize_turn` covering each outcome.

Validation:

- [ ] No regressions in existing `cargo test -p alleycat-hermes-bridge`.
- [ ] Fake-gateway test with terminal event in trailing chunk now reports correct status.
- [ ] Probe-driven manual test: `turn/start` happy path, deliberate gateway-side error, and `turn/interrupt` all produce exactly one `turn/completed`.

## Phase 2 — Add Durable Hermes Run State

### 2.1 Create run state module

Add a new module:

- `crates/hermes-bridge/src/run_state.rs`

Responsibilities:

- Store active and completed run records.
- Persist records to `<state_dir>/runs.json`.
- Provide lookup by `thread_id`, `turn_id`, and `run_id`.
- Track accumulated assistant text and terminal status.

API sketch:

```rust
pub struct RunStore { ... }

impl RunStore {
    pub fn open(path: PathBuf) -> anyhow::Result<Self>;
    pub fn upsert(&self, record: HermesRunRecord) -> anyhow::Result<()>;
    pub fn get_by_thread_turn(&self, thread_id: &str, turn_id: &str) -> Option<HermesRunRecord>;
    pub fn active_for_thread(&self, thread_id: &str) -> Option<HermesRunRecord>;
    pub fn mark_running(&self, turn_id: &str, run_id: &str) -> anyhow::Result<()>;
    pub fn mark_terminal(&self, run_id: &str, status: HermesTurnStatus, error: Option<String>) -> anyhow::Result<()>;
}
```

### 2.2 Persist state before streaming

In `crates/hermes-bridge/src/bridge.rs::handle_turn_start`:

- [ ] Create a run record with status `Starting` before `POST /v1/runs`.
- [ ] After `POST /v1/runs` succeeds, update with `run_id` and status `Running`.
- [ ] Only then spawn/connect the event pump.
- [ ] If `POST /v1/runs` fails, mark the run `Failed` and emit `turn/completed` with error (via `finalize_turn` from Phase 1.5).

### 2.3 Add `EventStore` and persist normalized events

- `crates/hermes-bridge/src/event_store.rs`

```rust
pub struct EventStore { dir: PathBuf, ... }

impl EventStore {
    pub fn open(dir: PathBuf) -> anyhow::Result<Self>;
    pub fn append(&self, run_id: &str, event: NormalizedHermesEvent) -> anyhow::Result<u64>;
    pub fn read_all(&self, run_id: &str) -> anyhow::Result<Vec<NormalizedHermesEvent>>;
    pub fn read_since(&self, run_id: &str, seq: u64) -> anyhow::Result<Vec<NormalizedHermesEvent>>;
}
```

- [ ] In the existing per-turn pump (still pre-Phase 3), write each normalized event to `EventStore::append` **before** the matching `ctx.notifier().send_notification`. This way replay never out-runs durability.

### 2.4 Load state at startup

In `HermesBridge::new`:

- [ ] Open `RunStore` and `EventStore` from `state_dir` (already wired).
- [ ] For records in `Running` / `Starting` state at startup, mark them `Unknown`.
  - Reconciliation (attempt reattach vs. mark `Cancelled`) is Phase 4.

Validation:

- [ ] Start Hermes thread/turn; confirm `runs.json` is written and `events/<run_id>.jsonl` is appended.
- [ ] Kill/restart Alleycat; confirm prior Hermes thread/run metadata remains readable.
- [ ] `thread/read` and `thread/turns/list` include completed turns reconstructed from the event log.

## Phase 3 — Introduce HermesRunManager

### 3.1 Add manager module

Add:

- `crates/hermes-bridge/src/run_manager.rs`

Responsibilities (shape depends on Phase 0 gating decision for SSE reopen):

- Ensure at most one SSE pump per active `run_id`.
- Broadcast normalized events to all connection subscribers.
- Append to `EventStore` exactly once per event.
- Reconnect to Hermes SSE when supported, otherwise mark run `Unknown` on pump death.

API sketch:

```rust
pub struct HermesRunManager {
    client: Arc<HermesApiClient>,
    run_store: Arc<RunStore>,
    event_store: Arc<EventStore>,
    active: DashMap<String /* run_id */, ActiveRunHandle>,
}

impl HermesRunManager {
    pub async fn start_or_attach_run(&self, run: HermesRunRecord) -> anyhow::Result<()>;
    pub fn subscribe(&self, run_id: &str) -> broadcast::Receiver<NormalizedHermesEvent>;
    pub async fn replay_since(&self, run_id: &str, seq: Option<u64>) -> anyhow::Result<Vec<NormalizedHermesEvent>>;
    pub async fn stop(&self, run_id: &str) -> anyhow::Result<()>;
}
```

Use `tokio::sync::broadcast` for live subscribers and the per-run JSONL for replay.

### 3.2 Normalize events once

Move event translation from the connection-specific pump into manager-level normalization.

Raw Hermes events:

- `message.delta`
- `run.completed`
- `run.failed`
- `run.cancelled`
- `approval.request`

Normalized events:

- `item/started`
- `agentMessage/delta`
- `item/completed`
- `turn/completed`
- `approval/requested` (forwarded to Phase 6)

The connection layer only translates normalized events into `ctx.notifier()` calls.

### 3.3 Handle terminal states once

- [ ] Ensure terminal events update `RunStore` exactly once (idempotent on `run_id`).
- [ ] Ensure repeated subscribers do not duplicate `RunStore` mutations.
- [ ] Ensure `turn/completed` is in `EventStore` so replay reproduces it.

Validation:

- [ ] Two simultaneous probe/client connections subscribed to the same run both receive deltas.
- [ ] A completed run has exactly one terminal record in `runs.json` and exactly one `turn/completed` entry per run in the event log.
- [ ] Event log replay reproduces `thread/read` / `thread/turns/list` output for a completed turn.

## Phase 4 — Reconnect and Mid-flight Reattach

### 4.1 Define reconnect semantics

For the first implementation:

- **Same-process, in-window reconnect**: already works via bridge-core's `ReplayRing` and `_alleycat_seq`. No new code needed beyond Phase 3's broadcast (so a second subscriber from a reconnected `Conn` actually receives live events).
- **Same-process, ring-evicted reconnect**: replay from `EventStore` for the active run, then live-tail from the broadcast.
- **Daemon-restart reconnect**: rehydrate `RunStore`; for any `Running`/`Starting` run, attempt SSE reattach if Phase 0 says it's supported, else mark `Unknown` and surface a clear status to the client.

Explicitly do not promise:

- attaching to arbitrary external Hermes runs not started by Alleycat;
- perfect replay if Hermes gateway does not support event replay and Alleycat was down during emitted events.

### 4.2 Hook into Alleycat session replay

Alleycat already passes `last_seen` through:

- `crates/alleycat/src/agents.rs::serve_agent_with_session`
- `crates/alleycat/src/agents.rs::serve_with_session`
- `alleycat_bridge_core::serve_stream_with_session`

Tasks:

- [ ] Confirm in code review that the existing `serve_stream_with_session` reattach path satisfies the same-process in-window case for Hermes. Add a test that exercises it via a fake gateway.
- [ ] When the ring is in drift (`AttachOutcome::DriftReload`), have the bridge respond to the next relevant client method (`thread/resume` typically) by reading from `EventStore` and re-emitting normalized events through `ctx.notifier()`. Bridge-core will re-stamp them with new `_alleycat_seq`.

### 4.3 Reattach active run on `thread/resume`

When `thread/resume` is called:

- [ ] Check `RunStore` for active run associated with `thread_id`.
- [ ] If found, ensure `HermesRunManager` has an active pump for `run_id` (or fall back per Phase 0 gating).
- [ ] Return thread with active/in-progress turn status.
- [ ] Replay persisted events to the connecting `Conn` if needed.

### 4.4 Reattach active run on reconnect without explicit resume

Depending on Litter client behavior, reconnect may attach to the same Alleycat session before calling `thread/resume`.

- [ ] Inspect actual client method sequence from logs/probe.
- [ ] If needed, subscribe connection to active run during initialize or first relevant method.

Validation:

- [ ] Start a long Hermes run from Litter.
- [ ] Disconnect client/network mid-run.
- [ ] Reconnect before run completes:
  - within ring window: bridge-core replay covers it, no `EventStore` read needed;
  - outside ring window: `EventStore` replay fills the gap.
- [ ] Verify deltas are not duplicated unexpectedly and new deltas continue.
- [ ] Verify terminal completion arrives after reconnect.
- [ ] Kill Alleycat mid-run, restart, reconnect: behavior matches Phase 0 gating decision (reattach SSE if supported; otherwise `Unknown` with clean message).

## Phase 5 — Improve Thread and History Semantics

### 5.1 Thread list/history from persisted bridge state

Current Hermes `thread/list` returns only bridge-created threads from `ThreadIndex`.

- [ ] Ensure `thread/list` includes status for active/running Hermes turns (join with `RunStore`).
- [ ] Ensure `thread/read?include_turns=true` reconstructs turns from persisted event/run logs (not just in-memory `turns` map).
- [ ] Ensure `thread/turns/list` returns turns across daemon restart.

### 5.2 Optional gateway import

Only if Phase 0 confirmed a listing endpoint exists:

- [ ] Add an import/hydration step on bridge startup.
- [ ] Map gateway sessions to Alleycat `HermesBinding` records.
- [ ] Avoid duplicating existing bindings by `hermes_session_id`.
- [ ] Mark imported threads with `source: appServer` and `agentNickname: hermes`.

If no listing endpoint exists:

- [ ] Document that only Alleycat-created Hermes bridge threads appear in Litter.

Validation:

- [ ] Create multiple Hermes threads from Litter.
- [ ] Restart Alleycat.
- [ ] `thread/list` still shows them.
- [ ] `thread/read` shows prior turns reconstructed from `EventStore`.

## Phase 6 — Approvals and Tool Events

Current handling:

- `approval.request` auto-approves only when `approval_policy == Never`.
- Otherwise it fails the turn with "Hermes approval required".

Bridge approvals like `pi-bridge` does, using the existing server→client request machinery.

Touchpoints (not just "wire it up"):

- [ ] Choose the **Codex method name** to use for the prompt (e.g. reuse `applyPatch/approvalRequest` if the shape matches, otherwise a custom `hermes/approvalRequest`).
- [ ] Use `NotificationSender::request(...)` — **not** `send_notification` — so the session's `pending`/`outstanding` tables are populated. This is what makes reattach replay the prompt via `serverRequest/replay`.
- [ ] Handle `ServerRequestError::ConnectionClosed` (no client to ask), `TimedOut`, and `Rpc(...)` distinctly.
- [ ] Extend `HermesApiClient::approve_run_once` (or add a sibling) to accept a decision payload, not the hard-coded `{"choice":"once"}`. Support at least:
  - approve once,
  - deny/cancel,
  - approve-always if Phase 0 confirmed gateway support.
- [ ] POST the chosen decision back to Hermes.
- [ ] If the client disconnects while an approval is pending: don't silently auto-deny; leave the outstanding request in the table until `pending_grace` expires per session policy, then post a denial to the gateway and emit a failed turn.

Relevant references:

- `crates/pi-bridge/src/approval.rs`
- `crates/pi-bridge/src/handlers/turn.rs`
- `crates/opencode-bridge/src/translate/events.rs`

Validation:

- [ ] Trigger a Hermes tool approval request.
- [ ] Approve from client; run continues.
- [ ] Deny from client; run stops or reports a controlled error.
- [ ] Disconnect during approval, reconnect within grace: client gets `serverRequest/replay` and can answer.
- [ ] Disconnect during approval, grace expires: gateway gets a deny; turn ends Failed; no orphaned pending state.

## Phase 7 — CLI Fallback Hardening

The CLI fallback should be explicit and predictable. (The user-facing `mode` config was already added in Phase 1.1.)

- [ ] Verify `crates/hermes-bridge/src/cli_adapter.rs` behavior with current Hermes CLI.
- [ ] Ensure CLI fallback receives cwd/session information when available.
- [ ] Add structured logs for fallback:
  - gateway health failed (reason),
  - fallback binary path,
  - command exit status,
  - duration.
- [ ] Document explicitly that CLI fallback is non-streaming and synthesizes a single completion message — it is not feature-equivalent to the API path.

Validation:

- [ ] Gateway up: API path used.
- [ ] Gateway down + CLI available + `mode=auto`: CLI path used with a single warning log per turn (not per probe).
- [ ] Gateway down + `mode=api`: user-visible error.
- [ ] `mode=cli`: no gateway probe required for turn execution.

## Phase 8 — Telemetry and Observability

Add structured logs across the Hermes path.

### Required fields

Include whenever relevant:

- `agent = "hermes"`
- `api_base`
- `thread_id`
- `turn_id`
- `hermes_session_id`
- `run_id`
- `event_name`
- `seq` (clarify which: `_alleycat_seq` or `event.seq`)
- `latency_ms`
- `error`
- `fallback_mode`

### Lifecycle events

- [ ] Bridge constructed.
- [ ] Health check started/completed/failed (with cache hit/miss).
- [ ] Thread created/resumed/listed.
- [ ] Run create started/completed/failed.
- [ ] SSE stream connected/disconnected/reconnecting/failed.
- [ ] Event normalized/persisted/replayed (from `EventStore` vs from `ReplayRing`).
- [ ] Turn terminal event observed.
- [ ] Approval requested/resolved/failed.
- [ ] CLI fallback selected/completed/failed.

### Metrics/tracing if supported

If the surrounding codebase has OpenTelemetry hooks later, add spans around:

- `hermes.health`
- `hermes.run.create`
- `hermes.events.stream`
- `hermes.event.persist`
- `hermes.replay`

Validation:

- [ ] Local daemon logs answer:
  - what run started,
  - which thread/session it belongs to,
  - how long run creation took,
  - whether SSE connected,
  - why a turn failed,
  - whether a replay came from ring or `EventStore`.

## Phase 9 — Testing Strategy

### Unit tests

Add or extend tests in `crates/hermes-bridge`.

- [ ] `api_client` tests for health/run/events/stop/approval response parsing.
- [ ] SSE parser tests for:
  - partial chunks,
  - multiple events per chunk,
  - terminal events,
  - **terminal event in trailing un-`\n\n`-terminated chunk** (regression for Phase 1.5 fix),
  - malformed JSON.
- [ ] `RunStore` persistence tests (round-trip, idempotent terminal write).
- [ ] `EventStore` append/replay tests, including `read_since`.
- [ ] `HermesRunManager` duplicate-pump prevention tests.
- [ ] `finalize_turn` tests for each outcome.
- [ ] Health cache TTL tests.

### Integration-style tests with fake gateway

Use the existing `axum`-style or hand-rolled `tokio::net::TcpListener` fake gateway pattern already used in `api_client.rs::tests`.

Scenarios:

- [ ] Healthy gateway returns `status: ok`.
- [ ] `thread/start` then `turn/start` creates a run; `runs.json` and `events/<run_id>.jsonl` are written.
- [ ] SSE deltas emit Codex-style notifications.
- [ ] Terminal `run.completed` emits exactly one `turn/completed`.
- [ ] `run.failed` emits failed turn with error message preserved.
- [ ] Gateway unavailable in `mode=api` returns error; no CLI invocation.
- [ ] Gateway unavailable in `mode=auto` falls back to CLI where a fake CLI is injected.
- [ ] Client reconnect within ring window: bridge-core replay covers it.
- [ ] Client reconnect outside ring window: `EventStore` replay fills the gap, no duplicates with bridge-core path.
- [ ] Daemon restart mid-run + reattach (gating-dependent).
- [ ] Approval request: approve, deny, disconnect-and-reconnect-with-replay, disconnect-and-grace-expires.

### Manual validation commands

```bash
cargo test -p alleycat-hermes-bridge
cargo test -p alleycat
cargo test --workspace
cargo install --locked --path crates/alleycat
systemctl --user restart alleycat.service
alleycat status --json    # confirmed: prints `agents[].available` per agent
alleycat probe --agent hermes --method thread/list --params '{}'
```

Manual turn test:

```bash
alleycat probe \
  --agent hermes \
  --method thread/start \
  --params '{"cwd":"/home/shuv/repos/alleycat","model":"hermes-agent"}'
```

Then use returned `thread.id` for:

```bash
alleycat probe \
  --agent hermes \
  --method turn/start \
  --params '{"threadId":"THREAD_ID","input":[{"type":"text","text":"Say hello from Hermes."}]}' \
  --linger-secs 20    # confirmed: probe.rs flag, default 5s
```

## Phase 10 — Documentation

- [ ] Update `README.md` Hermes section if present.
- [ ] Update `AGENTS.md` operational notes with:
  - Hermes gateway expected URL/port,
  - health check command,
  - `mode` configuration (auto/api/cli),
  - fallback behavior and non-streaming caveat,
  - known reconnect limitations per Phase 0 gating outcomes,
  - layout of `<state_dir>/hermes-bridge/{threads.json,runs.json,events/}`.
- [ ] Add troubleshooting section:
  - gateway down,
  - auth key missing,
  - SSE disconnect,
  - thread list empty because only bridge-created threads are indexed,
  - daemon-restart-while-running behavior.

## Risk Analysis

| Risk | Impact | Mitigation |
|---|---:|---|
| Hermes gateway SSE does not support replay after disconnect | Mid-flight recovery loses events during daemon downtime | Persist events to `EventStore` while Alleycat is alive; clearly document limits; reconnect only for future events if gateway permits |
| Event duplication on reconnect | Client UI shows duplicate deltas/items | Two-tier seq taxonomy; replay only events after known `event.seq`; terminal events idempotent in `RunStore` |
| Conflation of `_alleycat_seq` and `event.seq` | Subtle replay bugs, hard to debug | Documented seq taxonomy section; log fields name which seq is which |
| Tracer-bullet persistence missing an exit path | Inconsistent `RunStore` state | Phase 1.5 extracts `finalize_turn` so there is exactly one site to update |
| Thread index schema migration breaks existing users | Lost bridge-created Hermes threads | Additive `runs.json` and `events/` only; `threads.json` schema untouched |
| CLI fallback masks gateway outage | Confusing behavior and inconsistent features | Log fallback loudly; support explicit `mode = "api"` from Phase 1.1 |
| Long-lived SSE tasks leak | Memory/process growth | `HermesRunManager` owns one task per active `run_id`; terminal cleanup; tests for task removal |
| Unsupported approvals block real work | Hermes tool use fails | Phase 6 wires full request/response replay; honest fallback to deny if grace expires |
| Health-check spam from `list_agents` + per-turn probe | Wasted RTTs, noisy logs | Shared `HealthCache` with short TTL across both call sites (Phase 1.3) |

## Suggested Implementation Order

1. **Phase 0**: Characterize Hermes gateway API and reconnect behavior. Fill gating table.
2. **Phase 1**: Availability diagnostics + user-selectable `mode` + shared health cache.
3. **Phase 1.5**: Pump refactor (`finalize_turn`, trailing-chunk fix, dead branch, typed params).
4. **Phase 2**: Add `RunStore` + `EventStore`; persist via existing per-turn pump.
5. **Phase 3**: Introduce `HermesRunManager`; move ownership of SSE there; broadcast to subscribers.
6. **Phase 4**: Reconnect/replay semantics layered on top.
7. **Phase 5**: Improve history/thread reconstruction.
8. **Phase 6**: Approval bridging via `notifier().request(...)`.
9. **Phase 7**: CLI fallback hardening / docs (config moved earlier).
10. **Phase 8**: Telemetry pass.
11. **Phase 9/10**: Tests and documentation.

## First Tracer Bullet

A narrow first PR should deliver, in order:

- [ ] Phase 1.5 pump refactor (`finalize_turn`, trailing-chunk fix).
- [ ] Phase 1.1 user-selectable `mode` (small config change).
- [ ] `RunStore` persisted to `<state_dir>/hermes-bridge/runs.json`.
- [ ] `EventStore` JSONL persisted to `<state_dir>/hermes-bridge/events/<run_id>.jsonl`.
- [ ] `turn/start` persists `Starting -> Running -> Completed/Failed` (single call site via `finalize_turn`).
- [ ] Existing per-turn SSE pump writes normalized events to `EventStore` before sending notifications.
- [ ] `thread/read` and `thread/turns/list` reconstruct completed turns from persisted state.
- [ ] Structured logs for run creation, SSE connect, terminal event, and errors.
- [ ] Tests with fake gateway for successful streaming, failed run, terminal-in-trailing-chunk, and post-restart `thread/read`.

This avoids the larger `HermesRunManager` refactor while establishing durable state on a refactored pump that's safe to extend. Once durable event logs exist, Phase 3/4 can move live SSE ownership into `HermesRunManager` with less risk.

## Definition of Done

Hermes support should be considered materially improved when:

- [ ] Gateway-backed Hermes turns stream reliably to Litter.
- [ ] Bridge-created Hermes threads survive Alleycat restart.
- [ ] Completed turn history is visible after restart, reconstructed from `EventStore`.
- [ ] Disconnect/reconnect during an active run replays persisted events and continues streaming future events when gateway support allows, with same-process in-window reconnect handled by bridge-core's existing ring.
- [ ] Gateway failure and CLI fallback behavior are explicit, user-configurable via `mode`, and logged.
- [ ] Tests cover API success, API failure, terminal-in-trailing-chunk, SSE terminal states, persistence, ring replay, and `EventStore` replay.
- [ ] Operational docs explain how to run, test, and troubleshoot Hermes support, including the seq taxonomy and the gating-dependent limits from Phase 0.
