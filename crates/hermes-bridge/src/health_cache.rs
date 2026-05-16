//! Short-TTL availability cache for the Hermes gateway `/health` endpoint.
//!
//! Shared between `AgentManager::hermes_available()` (called per
//! `list_agents`) and per-turn dispatch in `HermesBridge`, both of which
//! would otherwise hit the gateway on every call.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::api_client::HermesApiClient;

#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub healthy: bool,
    pub reason: Option<String>,
}

struct CacheEntry {
    snapshot: HealthSnapshot,
    checked_at: Instant,
}

pub struct HealthCache {
    ttl: Duration,
    inner: Mutex<Option<CacheEntry>>,
}

impl HealthCache {
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            ttl: Duration::from_millis(ttl_ms),
            inner: Mutex::new(None),
        }
    }

    /// Return a cached value if it's within TTL, otherwise probe and cache.
    /// Probing happens with the given timeout — independent of the cache TTL.
    pub async fn get_or_probe(&self, client: &HermesApiClient, timeout_ms: u64) -> HealthSnapshot {
        if let Some(snapshot) = self.peek() {
            return snapshot;
        }
        let snapshot = probe(client, timeout_ms).await;
        self.insert(snapshot.clone());
        snapshot
    }

    pub fn peek(&self) -> Option<HealthSnapshot> {
        let inner = self.inner.lock().unwrap();
        inner.as_ref().and_then(|entry| {
            if entry.checked_at.elapsed() <= self.ttl {
                Some(entry.snapshot.clone())
            } else {
                None
            }
        })
    }

    pub fn insert(&self, snapshot: HealthSnapshot) {
        *self.inner.lock().unwrap() = Some(CacheEntry {
            snapshot,
            checked_at: Instant::now(),
        });
    }

    #[allow(dead_code)]
    pub fn invalidate(&self) {
        *self.inner.lock().unwrap() = None;
    }
}

async fn probe(client: &HermesApiClient, timeout_ms: u64) -> HealthSnapshot {
    let timeout = Duration::from_millis(timeout_ms.max(50));
    match tokio::time::timeout(timeout, client.health()).await {
        Ok(Ok(resp)) => {
            if resp.status.eq_ignore_ascii_case("ok") {
                HealthSnapshot {
                    healthy: true,
                    reason: None,
                }
            } else {
                HealthSnapshot {
                    healthy: false,
                    reason: Some(format!("status={}", resp.status)),
                }
            }
        }
        Ok(Err(err)) => HealthSnapshot {
            healthy: false,
            reason: Some(err.to_string()),
        },
        Err(_) => HealthSnapshot {
            healthy: false,
            reason: Some(format!("health probe timeout after {timeout_ms}ms")),
        },
    }
}
