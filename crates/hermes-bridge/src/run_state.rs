//! Durable Hermes run state.
//!
//! Persists per-run metadata to `<state_dir>/runs.json`. Each `HermesRunRecord`
//! tracks the lifecycle of a single Hermes gateway run (Starting → Running →
//! Completed/Failed/Cancelled). The store is the source of truth for "what
//! turns existed across daemon restarts" and is keyed by `(thread_id, turn_id)`
//! plus a secondary index on `run_id`.
//!
//! Atomic writes use the standard `tmp + rename` pattern.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Lifecycle status of a Hermes run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HermesTurnStatus {
    /// Persisted record but `POST /v1/runs` has not yet succeeded.
    Starting,
    /// `POST /v1/runs` succeeded and SSE pump is running (or scheduled).
    Running,
    /// Run reached `run.completed`.
    Completed,
    /// Run reached `run.failed` or any Alleycat-side fatal error.
    Failed,
    /// Run was cancelled via `turn/interrupt` or `run.cancelled`.
    Cancelled,
    /// Record exists but state is indeterminate (e.g. recovered after daemon
    /// restart while in `Running`).
    Unknown,
}

impl HermesTurnStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            HermesTurnStatus::Completed | HermesTurnStatus::Failed | HermesTurnStatus::Cancelled
        )
    }
}

/// Durable per-run record.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HermesRunRecord {
    pub thread_id: String,
    pub turn_id: String,
    pub hermes_session_id: String,
    #[serde(default)]
    pub run_id: Option<String>,
    pub status: HermesTurnStatus,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub completed_at: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub accumulated_text: String,
    /// Highest event seq this run has appended (via `EventStore`).
    #[serde(default)]
    pub last_event_seq: Option<u64>,
    /// Stable item id chosen for the agent message thread item. Lets
    /// reconnects emit the same `item/started`/`item/completed` correlation
    /// id as the original turn.
    #[serde(default)]
    pub agent_item_id: Option<String>,
    /// User-authored input items for durable post-restart turn reconstruction.
    #[serde(default)]
    pub user_items: Vec<alleycat_codex_proto::items::ThreadItem>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedRuns {
    records: Vec<HermesRunRecord>,
}

#[derive(Default)]
struct Inner {
    /// Keyed by `turn_id` (unique across the bridge).
    by_turn: HashMap<String, HermesRunRecord>,
    /// Secondary index: `run_id` → `turn_id`.
    by_run: HashMap<String, String>,
}

/// Thread-safe (optionally persisted) run store.
pub struct RunStore {
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl RunStore {
    pub fn new_in_memory() -> Self {
        Self {
            path: None,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn open_sync(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let persisted: PersistedRuns = match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)?,
            _ => PersistedRuns::default(),
        };
        let mut inner = Inner::default();
        for record in persisted.records {
            if let Some(ref run_id) = record.run_id {
                inner.by_run.insert(run_id.clone(), record.turn_id.clone());
            }
            inner.by_turn.insert(record.turn_id.clone(), record);
        }
        Ok(Self {
            path: Some(path),
            inner: Mutex::new(inner),
        })
    }

    /// Insert or replace by `turn_id`. Persists synchronously when a path is
    /// configured.
    pub fn upsert(&self, record: HermesRunRecord) -> anyhow::Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            // Remove stale run_id mapping if this turn previously had a different one.
            let prev_run_id = inner
                .by_turn
                .get(&record.turn_id)
                .and_then(|prev| prev.run_id.clone());
            if let Some(prev_run) = prev_run_id {
                inner.by_run.remove(&prev_run);
            }
            if let Some(ref run_id) = record.run_id {
                inner.by_run.insert(run_id.clone(), record.turn_id.clone());
            }
            inner.by_turn.insert(record.turn_id.clone(), record);
        }
        self.persist()
    }

    pub fn get_by_turn(&self, turn_id: &str) -> Option<HermesRunRecord> {
        self.inner.lock().unwrap().by_turn.get(turn_id).cloned()
    }

    pub fn get_by_run(&self, run_id: &str) -> Option<HermesRunRecord> {
        let inner = self.inner.lock().unwrap();
        inner
            .by_run
            .get(run_id)
            .and_then(|turn_id| inner.by_turn.get(turn_id).cloned())
    }

    /// Most-recent non-terminal run for a thread, if any.
    pub fn active_for_thread(&self, thread_id: &str) -> Option<HermesRunRecord> {
        let inner = self.inner.lock().unwrap();
        inner
            .by_turn
            .values()
            .filter(|r| r.thread_id == thread_id && !r.status.is_terminal())
            .max_by_key(|r| r.updated_at)
            .cloned()
    }

    /// All records for a thread, sorted by `created_at` ascending.
    pub fn list_for_thread(&self, thread_id: &str) -> Vec<HermesRunRecord> {
        let mut records: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .by_turn
            .values()
            .filter(|r| r.thread_id == thread_id)
            .cloned()
            .collect();
        records.sort_by_key(|r| r.created_at);
        records
    }

    /// Mark a turn as `Running` with its allocated `run_id`. Idempotent.
    pub fn mark_running(
        &self,
        turn_id: &str,
        run_id: &str,
        now: i64,
    ) -> anyhow::Result<Option<HermesRunRecord>> {
        {
            let mut inner = self.inner.lock().unwrap();
            // Read stale run_id without holding a mutable borrow.
            let prev_run_id = inner.by_turn.get(turn_id).and_then(|r| r.run_id.clone());
            if let Some(prev) = prev_run_id {
                if prev != run_id {
                    inner.by_run.remove(&prev);
                }
            }
            let Some(record) = inner.by_turn.get_mut(turn_id) else {
                return Ok(None);
            };
            record.run_id = Some(run_id.to_string());
            record.status = HermesTurnStatus::Running;
            record.updated_at = now;
            inner.by_run.insert(run_id.to_string(), turn_id.to_string());
        }
        self.persist()?;
        Ok(self.get_by_turn(turn_id))
    }

    /// Apply a terminal status. Idempotent on repeated terminal writes.
    pub fn mark_terminal(
        &self,
        turn_id: &str,
        status: HermesTurnStatus,
        error: Option<String>,
        accumulated_text: Option<String>,
        now: i64,
    ) -> anyhow::Result<Option<HermesRunRecord>> {
        debug_assert!(status.is_terminal());
        {
            let mut inner = self.inner.lock().unwrap();
            let Some(record) = inner.by_turn.get_mut(turn_id) else {
                return Ok(None);
            };
            // If already terminal, keep first-write-wins semantics.
            if !record.status.is_terminal() {
                record.status = status;
                record.error = error;
                if let Some(text) = accumulated_text {
                    record.accumulated_text = text;
                }
                record.completed_at = Some(now);
            }
            record.updated_at = now;
        }
        self.persist()?;
        Ok(self.get_by_turn(turn_id))
    }

    /// Update accumulated text (best-effort, called periodically by the pump).
    pub fn touch_text(&self, turn_id: &str, text: &str, now: i64) -> anyhow::Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            let Some(record) = inner.by_turn.get_mut(turn_id) else {
                return Ok(());
            };
            record.accumulated_text = text.to_string();
            record.updated_at = now;
        }
        self.persist()
    }

    /// Bump `last_event_seq`.
    pub fn note_event_seq(&self, turn_id: &str, seq: u64, now: i64) -> anyhow::Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            let Some(record) = inner.by_turn.get_mut(turn_id) else {
                return Ok(());
            };
            record.last_event_seq = Some(record.last_event_seq.map(|s| s.max(seq)).unwrap_or(seq));
            record.updated_at = now;
        }
        self.persist()
    }

    /// On startup, reconcile records left in non-terminal states. The bridge
    /// has no way to rejoin a Hermes SSE stream after restart (the gateway
    /// drops the queue when the consumer disconnects), so any prior `Running`
    /// / `Starting` records become `Unknown`. A best-effort terminal-status
    /// fetch can later flip them to `Completed`/`Failed` based on
    /// `GET /v1/runs/{run_id}` — that's handled by the run manager.
    pub fn mark_orphans_unknown(&self, now: i64) -> anyhow::Result<Vec<HermesRunRecord>> {
        let mut affected = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            for record in inner.by_turn.values_mut() {
                if matches!(
                    record.status,
                    HermesTurnStatus::Starting | HermesTurnStatus::Running
                ) {
                    record.status = HermesTurnStatus::Unknown;
                    record.updated_at = now;
                    affected.push(record.clone());
                }
            }
        }
        if !affected.is_empty() {
            self.persist()?;
        }
        Ok(affected)
    }

    fn persist(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let records: Vec<HermesRunRecord> = {
            let inner = self.inner.lock().unwrap();
            let mut records: Vec<_> = inner.by_turn.values().cloned().collect();
            records.sort_by(|a, b| a.turn_id.cmp(&b.turn_id));
            records
        };
        let data = PersistedRuns { records };
        let json = serde_json::to_string_pretty(&data)?;
        // Atomic write: write to tmp then rename.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn record(thread_id: &str, turn_id: &str, status: HermesTurnStatus) -> HermesRunRecord {
        HermesRunRecord {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            hermes_session_id: format!("ses_{turn_id}"),
            run_id: None,
            status,
            created_at: 1,
            updated_at: 1,
            completed_at: None,
            error: None,
            accumulated_text: String::new(),
            last_event_seq: None,
            agent_item_id: None,
            user_items: Vec::new(),
        }
    }

    #[test]
    fn roundtrip_persists_and_reloads() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("runs.json");
        let store = RunStore::open_sync(path.clone()).unwrap();
        store
            .upsert(record("thread-1", "turn-1", HermesTurnStatus::Starting))
            .unwrap();
        store.mark_running("turn-1", "run_abc", 2).unwrap();
        let reloaded = RunStore::open_sync(path).unwrap();
        let got = reloaded.get_by_turn("turn-1").unwrap();
        assert_eq!(got.status, HermesTurnStatus::Running);
        assert_eq!(got.run_id.as_deref(), Some("run_abc"));
        assert_eq!(got.updated_at, 2);
        // Secondary index by run_id.
        let by_run = reloaded.get_by_run("run_abc").unwrap();
        assert_eq!(by_run.turn_id, "turn-1");
    }

    #[test]
    fn mark_terminal_is_idempotent() {
        let store = RunStore::new_in_memory();
        store
            .upsert(record("t", "turn-x", HermesTurnStatus::Running))
            .unwrap();
        let first = store
            .mark_terminal(
                "turn-x",
                HermesTurnStatus::Completed,
                None,
                Some("hello".into()),
                10,
            )
            .unwrap()
            .unwrap();
        let second = store
            .mark_terminal(
                "turn-x",
                HermesTurnStatus::Failed,
                Some("late".into()),
                None,
                11,
            )
            .unwrap()
            .unwrap();
        // First write wins for status/error/text; only updated_at advances.
        assert_eq!(first.status, HermesTurnStatus::Completed);
        assert_eq!(second.status, HermesTurnStatus::Completed);
        assert_eq!(second.error, None);
        assert_eq!(second.accumulated_text, "hello");
        assert_eq!(second.updated_at, 11);
    }

    #[test]
    fn orphans_become_unknown() {
        let store = RunStore::new_in_memory();
        store
            .upsert(record("t", "turn-1", HermesTurnStatus::Running))
            .unwrap();
        store
            .upsert(record("t", "turn-2", HermesTurnStatus::Completed))
            .unwrap();
        let affected = store.mark_orphans_unknown(99).unwrap();
        assert_eq!(affected.len(), 1);
        assert_eq!(affected[0].turn_id, "turn-1");
        assert_eq!(
            store.get_by_turn("turn-1").unwrap().status,
            HermesTurnStatus::Unknown
        );
        assert_eq!(
            store.get_by_turn("turn-2").unwrap().status,
            HermesTurnStatus::Completed
        );
    }

    #[test]
    fn active_for_thread_picks_most_recent_non_terminal() {
        let store = RunStore::new_in_memory();
        let mut a = record("th", "turn-a", HermesTurnStatus::Completed);
        a.updated_at = 100;
        let mut b = record("th", "turn-b", HermesTurnStatus::Running);
        b.updated_at = 50;
        let mut c = record("th", "turn-c", HermesTurnStatus::Starting);
        c.updated_at = 200;
        store.upsert(a).unwrap();
        store.upsert(b).unwrap();
        store.upsert(c).unwrap();
        let active = store.active_for_thread("th").unwrap();
        assert_eq!(active.turn_id, "turn-c");
    }
}
