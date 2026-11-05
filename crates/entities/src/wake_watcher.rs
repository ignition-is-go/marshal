//! `WakeWatcher`: the daemon's record that a session is holding `/hook/wait`.
//!
//! This is separate from `Session` because a session writes its own `Session`
//! row, so a field there would only be the session's claim. The daemon's hook
//! listener writes this one. Without the gate, any client can also write one,
//! so treat it as a record, not proof. The entity `id` is the session id.

use std::sync::Arc;

use myko::myko_item;
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};

use crate::Session;

#[myko_item]
pub struct WakeWatcher {
    /// When this record stops counting, in ms since the epoch. A waiter that
    /// vanishes without the daemon noticing stops counting by itself. Records
    /// aren't persisted, because a wait doesn't outlive the daemon.
    pub until: i64,
}

/// Whether a message would wake a session, as far as Marshal can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeState {
    /// Observed: the daemon has a current waiter for this session.
    Waiting,
    /// Declared: the session says it has a live channel. Not observed.
    Channel,
    /// Declared: it reports a turn in progress. Claude Code's wait hook starts
    /// again when the turn ends, so having no waiter now isn't a fault.
    Busy,
    /// No sign that a message would wake it. It'll see the mail on its next turn,
    /// if it has one.
    None,
}

impl WakeState {
    pub fn as_str(self) -> &'static str {
        match self {
            WakeState::Waiting => "waiting",
            WakeState::Channel => "channel",
            WakeState::Busy => "busy",
            WakeState::None => "none",
        }
    }
}

/// Work out a session's wake state. Pure, so each reader gets the same answer.
pub fn wake_state(session: &Session, watchers: &[Arc<WakeWatcher>], now_ms: i64) -> WakeState {
    let waiting = watchers
        .iter()
        .any(|w| w.id.0.as_ref() == session.id.0.as_ref() && w.until > now_ms);
    if waiting {
        WakeState::Waiting
    } else if session.client_id.is_some() && session.channels_enabled == Some(true) {
        WakeState::Channel
    } else if session.activity.as_deref() == Some("busy") {
        WakeState::Busy
    } else {
        WakeState::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;
    use myko::entities::client::ClientId;

    fn session(id: &str) -> Session {
        Session {
            id: SessionId(Arc::from(id)),
            client_id: None,
            pid: 0,
            cwd: String::new(),
            git_branch: None,
            current_task: None,
            session_name: None,
            activity: None,
            kind: None,
            connected_at: 0,
            last_activity_at: None,
            last_tool: None,
            last_tool_at: None,
            operator: None,
            host: None,
            project: None,
            channels_enabled: None,
        }
    }

    fn watcher(id: &str, until: i64) -> Arc<WakeWatcher> {
        Arc::new(WakeWatcher {
            id: WakeWatcherId(Arc::from(id)),
            until,
        })
    }

    #[test]
    fn a_current_waiter_counts_and_an_expired_one_does_not() {
        let s = session("a");
        assert_eq!(
            wake_state(&s, &[watcher("a", 101)], 100),
            WakeState::Waiting
        );
        assert_eq!(wake_state(&s, &[watcher("a", 100)], 100), WakeState::None);
    }

    #[test]
    fn another_sessions_waiter_does_not_count() {
        assert_eq!(
            wake_state(&session("a"), &[watcher("b", 101)], 100),
            WakeState::None
        );
    }

    #[test]
    fn a_channel_needs_a_connection() {
        let mut s = session("a");
        s.channels_enabled = Some(true);
        assert_eq!(wake_state(&s, &[], 0), WakeState::None);
        s.client_id = Some(ClientId(Arc::from("c")));
        assert_eq!(wake_state(&s, &[], 0), WakeState::Channel);
    }

    #[test]
    fn a_busy_session_is_not_reported_as_unwakeable() {
        let mut s = session("a");
        s.activity = Some("busy".into());
        assert_eq!(wake_state(&s, &[], 0), WakeState::Busy);
    }
}
