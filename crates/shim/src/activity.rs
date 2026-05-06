//! Tracks MCP server activity so the self-update watcher knows when it's
//! safe to re-exec without corrupting an in-flight request/response.
//!
//! Doubles as the source of truth for the roster's liveness fields
//! (`Session.last_activity_at`, `last_tool`, `last_tool_at`): the shim
//! has a periodic 5s flusher that reads these atomics + the last-tool
//! mutex and pushes them upstream via the auto-generated setters.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

pub struct Activity {
    in_flight: AtomicU64,
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
            in_flight: AtomicU64::new(0),
            last_activity_ms: AtomicI64::new(now_ms()),
            last_tool: Mutex::new(None),
            last_tool_ms: AtomicI64::new(0),
        }
    }

    /// Mark the start of work (a tools/call dispatch). Pairs with `end`.
    pub fn start(&self) {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.bump();
    }

    /// Mark the end of work. Pairs with `start`.
    pub fn end(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.bump();
    }

    /// Touch the last-activity clock without changing the in-flight count.
    pub fn bump(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::SeqCst)
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

    /// True when no work is in flight and the last activity was at least
    /// `dur` ago. Used as the gate for re-exec.
    pub fn idle_for(&self, dur: Duration) -> bool {
        if self.in_flight() > 0 {
            return false;
        }
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        let elapsed = now_ms().saturating_sub(last);
        elapsed >= dur.as_millis() as i64
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_activity_is_not_idle() {
        let a = Activity::new();
        assert!(!a.idle_for(Duration::from_secs(1)));
    }

    #[test]
    fn becomes_idle_after_window() {
        let a = Activity::new();
        std::thread::sleep(Duration::from_millis(50));
        assert!(a.idle_for(Duration::from_millis(20)));
    }

    #[test]
    fn in_flight_blocks_idle() {
        let a = Activity::new();
        a.start();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!a.idle_for(Duration::from_millis(10)));
        a.end();
        // end() bumps last-activity, so we need to wait again.
        std::thread::sleep(Duration::from_millis(50));
        assert!(a.idle_for(Duration::from_millis(10)));
    }

    #[test]
    fn bump_resets_idle_window() {
        let a = Activity::new();
        std::thread::sleep(Duration::from_millis(50));
        assert!(a.idle_for(Duration::from_millis(30)));
        a.bump();
        assert!(!a.idle_for(Duration::from_millis(30)));
    }
}
