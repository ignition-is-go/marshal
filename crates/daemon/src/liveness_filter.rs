//! Durable-write filter for the high-churn `Session` liveness fields.
//!
//! The shim flushes liveness telemetry (`last_activity_at`, `last_tool`,
//! `last_tool_at`, `activity`) on a seconds-scale cadence, and every flush is
//! a full Session SET. Persisting each one appends a full-row event to the
//! durable store, so the event table grows with *heartbeat traffic* rather
//! than with meaningful session state — the production table reached millions
//! of rows this way (lv-6731) and startup catch-up could no longer fit in the
//! host's memory.
//!
//! This persister sits in front of the real durable persister for the
//! `Session` entity only. A SET whose *durable projection* — the item minus
//! the liveness fields — is unchanged from the last persisted SET is dropped;
//! everything else (identity changes, `current_task`, `git_branch`, reconnect
//! `clientId` moves, DELs) passes through. In-memory state and live
//! subscriptions are unaffected: this runs on the durable-production path
//! only, after the store and reactive graph have already applied the event.
//!
//! Consequence for replay: a replayed Session carries the liveness values
//! from its last durable-relevant change, not its last heartbeat. That is
//! already the post-restart contract — the cleanup sweeper grants every
//! replayed session a fresh reconnect grace and live shims re-push liveness
//! immediately on reconnect.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use myko::{
    server::{PersistError, PersistHealth, Persister},
    wire::{MEvent, MEventType},
};
use serde_json::Value;

/// Wire-format (camelCase) Session fields that carry liveness telemetry.
/// Changes confined to these fields are not worth a durable row.
const LIVENESS_FIELDS: [&str; 4] = ["lastActivityAt", "lastTool", "lastToolAt", "activity"];

/// Per-entity persister that forwards to `inner` only when a SET changes the
/// durable projection. The inner persister is late-bound because the server
/// builder constructs the Postgres producer inside `build()` — `bind()` it
/// from `CellServer::postgres_producer` right after. Until bound (or forever,
/// in ephemeral mode) events are dropped, matching the blackhole default.
#[derive(Default)]
pub struct SessionLivenessFilter {
    inner: OnceLock<Arc<dyn Persister>>,
    /// item id → durable projection of the last SET we forwarded.
    /// Bounded by live session count: DELs evict, and the cleanup sweeper
    /// DELs every abandoned session.
    last_durable: Mutex<HashMap<String, Value>>,
}

impl SessionLivenessFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Late-bind the real durable persister. Call once after server build;
    /// later calls are ignored.
    pub fn bind(&self, inner: Arc<dyn Persister>) {
        let _ = self.inner.set(inner);
    }

    /// The item with liveness fields removed — what "changed enough to
    /// persist" is judged against.
    fn durable_projection(item: &Value) -> Value {
        let mut projected = item.clone();
        if let Some(map) = projected.as_object_mut() {
            for field in LIVENESS_FIELDS {
                map.remove(field);
            }
        }
        projected
    }

    fn item_id(item: &Value) -> Option<String> {
        item.get("id").and_then(Value::as_str).map(str::to_owned)
    }
}

impl Persister for SessionLivenessFilter {
    fn persist(&self, event: MEvent) -> Result<(), PersistError> {
        let Some(inner) = self.inner.get() else {
            // Ephemeral mode (no durable store configured): drop, like the
            // blackhole default persister this entity would otherwise use.
            return Ok(());
        };
        let Some(id) = Self::item_id(&event.item) else {
            // No id to key the cache on — never invent a filter decision,
            // just persist.
            return inner.persist(event);
        };
        match event.change_type {
            MEventType::DEL => {
                self.last_durable.lock().unwrap().remove(&id);
                inner.persist(event)
            }
            MEventType::SET => {
                let projection = Self::durable_projection(&event.item);
                let mut cache = self.last_durable.lock().unwrap();
                if cache.get(&id) == Some(&projection) {
                    return Ok(());
                }
                cache.insert(id, projection);
                drop(cache);
                inner.persist(event)
            }
        }
    }

    fn health(&self) -> Arc<PersistHealth> {
        match self.inner.get() {
            Some(inner) => inner.health(),
            None => Persister::health(&myko_server::BlackholePersister),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Inner persister that counts what reaches it.
    #[derive(Default)]
    struct Counting {
        sets: AtomicUsize,
        dels: AtomicUsize,
    }

    impl Persister for Counting {
        fn persist(&self, event: MEvent) -> Result<(), PersistError> {
            match event.change_type {
                MEventType::SET => self.sets.fetch_add(1, Ordering::SeqCst),
                MEventType::DEL => self.dels.fetch_add(1, Ordering::SeqCst),
            };
            Ok(())
        }
    }

    fn session_set(id: &str, task: Option<&str>, last_activity_at: i64) -> MEvent {
        let mut item = serde_json::json!({
            "id": id,
            "pid": 42,
            "cwd": "/repo",
            "connectedAt": 1_700_000_000_000_i64,
            "lastActivityAt": last_activity_at,
        });
        if let Some(task) = task {
            item["currentTask"] = Value::String(task.to_string());
        }
        MEvent {
            item,
            change_type: MEventType::SET,
            item_type: "Session".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            tx: "tx".to_string(),
            source_id: None,
        }
    }

    fn session_del(id: &str) -> MEvent {
        MEvent {
            item: serde_json::json!({ "id": id }),
            change_type: MEventType::DEL,
            item_type: "Session".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            tx: "tx".to_string(),
            source_id: None,
        }
    }

    fn bound_filter() -> (SessionLivenessFilter, Arc<Counting>) {
        let counting = Arc::new(Counting::default());
        let filter = SessionLivenessFilter::new();
        filter.bind(counting.clone());
        (filter, counting)
    }

    #[test]
    fn liveness_only_churn_is_dropped_after_first_set() {
        let (filter, inner) = bound_filter();
        // First SET establishes the durable row.
        filter.persist(session_set("s1", None, 1)).unwrap();
        // Heartbeats: same durable projection, moving liveness.
        for t in 2..100 {
            filter.persist(session_set("s1", None, t)).unwrap();
        }
        assert_eq!(inner.sets.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn durable_field_change_is_persisted() {
        let (filter, inner) = bound_filter();
        filter.persist(session_set("s1", None, 1)).unwrap();
        filter
            .persist(session_set("s1", Some("reviewing"), 2))
            .unwrap();
        filter
            .persist(session_set("s1", Some("reviewing"), 3))
            .unwrap();
        filter
            .persist(session_set("s1", Some("testing"), 4))
            .unwrap();
        assert_eq!(inner.sets.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn del_forwards_and_resets_the_cache() {
        let (filter, inner) = bound_filter();
        filter.persist(session_set("s1", None, 1)).unwrap();
        filter.persist(session_del("s1")).unwrap();
        // Re-registration after a DEL must persist even though the durable
        // projection matches the pre-DEL row.
        filter.persist(session_set("s1", None, 2)).unwrap();
        assert_eq!(inner.dels.load(Ordering::SeqCst), 1);
        assert_eq!(inner.sets.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn sessions_are_filtered_independently() {
        let (filter, inner) = bound_filter();
        filter.persist(session_set("s1", None, 1)).unwrap();
        filter.persist(session_set("s2", None, 1)).unwrap();
        filter.persist(session_set("s1", None, 2)).unwrap();
        filter.persist(session_set("s2", None, 2)).unwrap();
        assert_eq!(inner.sets.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn unbound_filter_drops_like_the_blackhole_default() {
        let filter = SessionLivenessFilter::new();
        assert!(filter.persist(session_set("s1", None, 1)).is_ok());
        assert!(filter.persist(session_del("s1")).is_ok());
    }
}
