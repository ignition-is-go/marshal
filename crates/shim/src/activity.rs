//! Source of truth for the roster's liveness fields
//! (`Session.last_activity_at`, `last_tool`, `last_tool_at`): the shim
//! has a periodic 5s flusher that reads these atomics + the last-tool
//! mutex and pushes them upstream via the auto-generated setters.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

pub struct Activity {
    last_activity_ms: AtomicI64,
    /// Name of the most recent MCP tool dispatched, captured at
    /// `tools/call` time. No arguments — just the tool's name — so
    /// the roster surfaces "this worker last called X 3s ago"
    /// without leaking task-specific content. `None` until the
    /// shim's first tool call. Wrapped in a Mutex because String
    /// has no atomic equivalent and the read/write rate is one
    /// event per tool call (low frequency, no contention concern).
    last_tool: Mutex<Option<String>>,
    last_tool_ms: AtomicI64,
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

impl Activity {
    pub fn new() -> Self {
        Self {
            last_activity_ms: AtomicI64::new(now_ms()),
            last_tool: Mutex::new(None),
            last_tool_ms: AtomicI64::new(0),
        }
    }

    /// Mark the start of work (a tools/call dispatch). Pairs with `end`.
    pub fn start(&self) {
        self.bump();
    }

    /// Mark the end of work. Pairs with `start`.
    pub fn end(&self) {
        self.bump();
    }

    /// Touch the last-activity clock.
    pub fn bump(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Wall-clock millis of the most recent `start` / `end` / `bump`
    /// call. Exposed so the shim's roster-publish path can flush this
    /// timestamp upstream as `Session.last_activity_at` for the
    /// daemon's stale-worker detection.
    pub fn last_activity_ms(&self) -> i64 {
        self.last_activity_ms.load(Ordering::Relaxed)
    }

    /// Record the name of the MCP tool the dispatcher is about to
    /// invoke. Bumps `last_activity_ms` too because by definition a
    /// tool call counts as activity. Called from the dispatcher at
    /// `tools/call` time (paired with `start()` — same site).
    pub fn record_tool(&self, name: &str) {
        let now = now_ms();
        self.last_activity_ms.store(now, Ordering::Relaxed);
        self.last_tool_ms.store(now, Ordering::Relaxed);
        if let Ok(mut slot) = self.last_tool.lock() {
            *slot = Some(name.to_string());
        }
    }

    /// Snapshot the last-tool name. Used by the periodic flusher
    /// that publishes liveness fields to the daemon.
    pub fn last_tool_name(&self) -> Option<String> {
        self.last_tool.lock().ok().and_then(|slot| slot.clone())
    }

    /// Wall-clock millis of the most recent `record_tool` call. 0 if
    /// no tool has been served yet — callers map 0 → None.
    pub fn last_tool_ms(&self) -> i64 {
        self.last_tool_ms.load(Ordering::Relaxed)
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_tool_sets_name_and_timestamp() {
        let a = Activity::new();
        a.record_tool("send_message");
        assert_eq!(a.last_tool_name().as_deref(), Some("send_message"));
        assert!(a.last_tool_ms() > 0);
    }

    #[test]
    fn bump_advances_last_activity() {
        let a = Activity::new();
        let before = a.last_activity_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        a.bump();
        assert!(a.last_activity_ms() >= before);
    }
}
