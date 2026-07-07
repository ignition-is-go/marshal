//! Nickname resolution for the whole dashboard.
//!
//! The daemon assigns each session a sticky `adjective-noun` handle
//! (`SessionNickname`, keyed by session id) and every marshal surface — the
//! shims, the TUI, the message notifications — addresses peers by it. The
//! dashboard does the same: one live subscription at the root, read everywhere
//! via `use_nicknames()`, so a handle shows identically in the roster, the
//! message feed, the rooms list, and the detail pane.
//!
//! This READS the assignment (never recomputes it) — the same contract the
//! opencode plugin and the Rust shim moved to when nicknames became grandfathered
//! daemon state. Before a session's assignment saga has run (a brief window) we
//! fall back to its short id.

use leptos::prelude::*;
use marshal_entities::{GetAllSessionNicknames, SessionNickname};
use std::sync::Arc;

/// Handle to the live nickname map. Cheap to copy; `of()` is a reactive read.
#[derive(Clone, Copy)]
pub struct Nicknames(ReadSignal<Vec<Arc<SessionNickname>>>);

impl Nicknames {
    /// The daemon-assigned handle for `session_id`, or its short id if no
    /// assignment has landed yet. Reactive: re-runs when an assignment arrives.
    pub fn of(&self, session_id: &str) -> String {
        self.0
            .with(|all| {
                all.iter()
                    .find(|n| n.id.0.as_ref() == session_id)
                    .map(|n| n.nickname.clone())
            })
            .unwrap_or_else(|| short_id(session_id))
    }
}

/// Subscribe to the nickname store and stash the handle in context. Call once,
/// inside `App`, AFTER `provide_myko` (it opens a live query).
pub fn provide_nicknames() {
    let assigned = myko_leptos::live_query::<GetAllSessionNicknames>(|| GetAllSessionNicknames {});
    provide_context(Nicknames(assigned));
}

/// Read the nickname map from context. Panics only if `provide_nicknames()`
/// wasn't called at the root — a wiring bug, not a runtime condition.
pub fn use_nicknames() -> Nicknames {
    use_context::<Nicknames>().expect("provide_nicknames() must run at the app root")
}

/// First 8 chars of a session id — the fallback label before a nickname lands,
/// and a stable dim sub-label beside the handle.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}
