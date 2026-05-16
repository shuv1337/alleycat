//! Per-run normalized event log.
//!
//! Stores normalized bridge events to `<state_dir>/events/<run_id>.jsonl`,
//! one event per line. Per-run monotonic `seq` is assigned at append time and
//! returned to the caller. This is the **only** source of truth for daemon-
//! restart-survivable replay: the bridge-core `ReplayRing` evicts in-memory
//! frames, and the Hermes gateway `/v1/runs/{id}/events` endpoint is not
//! reopenable (Phase 0).
//!
//! Events are normalized bridge frames — i.e. they are the JSON-RPC
//! notification params we would send to the client. Re-emitting them through
//! `ctx.notifier()` after a restart causes bridge-core to re-stamp a new
//! `_alleycat_seq`, which is correct: the session sequence is independent of
//! the per-run sequence.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A normalized bridge event persisted for replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedHermesEvent {
    /// Monotonic, per-`run_id`, 1-based.
    pub seq: u64,
    /// Epoch milliseconds.
    pub ts: i64,
    pub thread_id: String,
    pub turn_id: String,
    pub run_id: String,
    /// JSON-RPC notification method, e.g. `item/started`, `item/agentMessage/delta`,
    /// `item/completed`, `turn/completed`, or a bridge-specific approval frame.
    pub method: String,
    /// JSON-RPC `params` value for the notification.
    pub params: Value,
}

#[derive(Default)]
struct PerRun {
    next_seq: u64,
}

/// Append-and-replay store for normalized Hermes events.
pub struct EventStore {
    dir: Option<PathBuf>,
    counters: Mutex<HashMap<String, PerRun>>,
}

impl EventStore {
    pub fn new_in_memory() -> Self {
        Self {
            dir: None,
            counters: Mutex::new(HashMap::new()),
        }
    }

    pub fn open_sync(dir: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(&dir)?;
        // Rehydrate per-run next_seq by scanning existing files.
        let mut counters: HashMap<String, PerRun> = HashMap::new();
        if let Ok(read_dir) = fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                let Some(stem) = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .and_then(|name| name.strip_suffix(".jsonl"))
                else {
                    continue;
                };
                let max_seq = scan_max_seq(&path).unwrap_or(0);
                counters.insert(
                    stem.to_string(),
                    PerRun {
                        next_seq: max_seq.saturating_add(1).max(1),
                    },
                );
            }
        }
        Ok(Self {
            dir: Some(dir),
            counters: Mutex::new(counters),
        })
    }

    /// Append a normalized event. `event.seq` is overwritten with the next
    /// per-run sequence and the assigned value is returned.
    pub fn append(
        &self,
        run_id: &str,
        mut event: NormalizedHermesEvent,
    ) -> anyhow::Result<NormalizedHermesEvent> {
        let seq = {
            let mut counters = self.counters.lock().unwrap();
            let slot = counters.entry(run_id.to_string()).or_default();
            slot.next_seq = slot.next_seq.max(1);
            let s = slot.next_seq;
            slot.next_seq = s.saturating_add(1);
            s
        };
        event.seq = seq;
        if let Some(dir) = &self.dir {
            let path = dir.join(format!("{run_id}.jsonl"));
            let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
            let line = serde_json::to_string(&event)?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_data().ok();
        }
        Ok(event)
    }

    /// Read all events for `run_id` from disk. Returns empty if no file
    /// exists or the store is in-memory only.
    pub fn read_all(&self, run_id: &str) -> anyhow::Result<Vec<NormalizedHermesEvent>> {
        let Some(dir) = &self.dir else {
            return Ok(Vec::new());
        };
        let path = dir.join(format!("{run_id}.jsonl"));
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let mut out = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<NormalizedHermesEvent>(&line) {
                Ok(ev) => out.push(ev),
                Err(err) => {
                    tracing::warn!(
                        run_id = %run_id,
                        error = %err,
                        "hermes event_store: skipping malformed jsonl line"
                    );
                }
            }
        }
        Ok(out)
    }

    /// Read events with `seq > after_seq`. Returns empty if no file or no
    /// matching events.
    pub fn read_since(
        &self,
        run_id: &str,
        after_seq: u64,
    ) -> anyhow::Result<Vec<NormalizedHermesEvent>> {
        let mut all = self.read_all(run_id)?;
        all.retain(|e| e.seq > after_seq);
        Ok(all)
    }
}

fn scan_max_seq(path: &Path) -> Option<u64> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut max = 0u64;
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<NormalizedHermesEvent>(&line) {
            if ev.seq > max {
                max = ev.seq;
            }
        }
    }
    Some(max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn ev(method: &str) -> NormalizedHermesEvent {
        NormalizedHermesEvent {
            seq: 0,
            ts: 1,
            thread_id: "t".into(),
            turn_id: "tu".into(),
            run_id: "run-1".into(),
            method: method.into(),
            params: json!({"hello":"world"}),
        }
    }

    #[test]
    fn append_assigns_monotonic_seq_and_persists() {
        let dir = TempDir::new().unwrap();
        let store = EventStore::open_sync(dir.path().to_path_buf()).unwrap();
        let e1 = store.append("run-1", ev("item/started")).unwrap();
        let e2 = store
            .append("run-1", ev("item/agentMessage/delta"))
            .unwrap();
        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        // Independent counter per run.
        let e_other = store.append("run-2", ev("item/started")).unwrap();
        assert_eq!(e_other.seq, 1);
        // Round-trip.
        let reloaded = EventStore::open_sync(dir.path().to_path_buf()).unwrap();
        let all = reloaded.read_all("run-1").unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].seq, 1);
        // Next append continues from max+1.
        let e3 = reloaded.append("run-1", ev("turn/completed")).unwrap();
        assert_eq!(e3.seq, 3);
    }

    #[test]
    fn read_since_filters_by_seq() {
        let dir = TempDir::new().unwrap();
        let store = EventStore::open_sync(dir.path().to_path_buf()).unwrap();
        for i in 0..5 {
            let mut e = ev("item/agentMessage/delta");
            e.params = json!({"i": i});
            store.append("run-x", e).unwrap();
        }
        let since = store.read_since("run-x", 3).unwrap();
        assert_eq!(since.len(), 2);
        assert_eq!(since[0].seq, 4);
        assert_eq!(since[1].seq, 5);
    }

    #[test]
    fn read_unknown_run_returns_empty() {
        let dir = TempDir::new().unwrap();
        let store = EventStore::open_sync(dir.path().to_path_buf()).unwrap();
        assert!(store.read_all("nope").unwrap().is_empty());
        assert!(store.read_since("nope", 0).unwrap().is_empty());
    }
}
