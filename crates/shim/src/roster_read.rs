//! `marshal-shim roster`: print the roster as JSON for other programs.
//!
//! It only subscribes to queries. It registers no session, so reading the roster
//! doesn't change it and the reader doesn't appear on it.
//!
//! Each session's facts are split by who set them:
//!
//! - `recorded`: the daemon's record. myko fills `client_id` from the
//!   connection, and the daemon's hook listener writes a `WakeWatcher` while
//!   `/hook/wait` is held. But any connected client can also write either: it
//!   can SET a `WakeWatcher`, or SET another session's id and rebind it. So
//!   these are the daemon's record, not proof, and even when true they show a
//!   connection or a wait, not that the session behind it is working.
//! - `declared`: what the session's own software reports, including its machine,
//!   operator and `last_activity_at`. Any client can set these to anything.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::Utc;
use hyphae::Gettable;
use marshal_entities::{
    GetAllSessionNicknames, GetAllSessions, GetAllWakeWatchers, Session, SessionNickname,
    WakeWatcher, wake_state,
};
use myko::client::MykoClient;
use serde_json::{Value, json};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);
/// How long to let the wake watchers arrive once the roster has. There may be
/// none, so this can't wait for them to appear.
const WATCHERS_SETTLE: Duration = Duration::from_secs(1);

fn view(
    sessions: &[Arc<Session>],
    nicknames: &[Arc<SessionNickname>],
    watchers: &[Arc<WakeWatcher>],
    now_ms: i64,
) -> Value {
    let rows: Vec<Value> = sessions
        .iter()
        .map(|s| {
            let id = s.id.0.as_ref();
            let nickname = nicknames
                .iter()
                .find(|n| n.id.0.as_ref() == id)
                .map(|n| n.nickname.clone())
                .unwrap_or_else(|| marshal_entities::nickname(id));
            let waiting_until = watchers
                .iter()
                .find(|w| w.id.0.as_ref() == id && w.until > now_ms)
                .map(|w| w.until);
            json!({
                "session_id": id,
                "nickname": nickname,
                "wake": wake_state(s, watchers, now_ms).as_str(),
                "recorded": {
                    // The row is bound to a connection. It can outlast that
                    // connection until the daemon's sweep reaps it.
                    "bound_to_connection": s.client_id.is_some(),
                    "waiting": waiting_until.is_some(),
                    "waiting_until": waiting_until,
                },
                "declared": {
                    "machine": s.host.as_ref().map(|h| h.name.as_str()),
                    "operator": s.operator,
                    "activity": s.activity,
                    "last_activity_at": s.last_activity_at,
                    "last_tool": s.last_tool,
                    "last_tool_at": s.last_tool_at,
                    "connected_at": s.connected_at,
                    "current_task": s.current_task,
                    "session_name": s.session_name,
                    "kind": s.kind,
                    "project": s.project,
                    "cwd": s.cwd,
                    "git_branch": s.git_branch,
                    "pid": s.pid,
                    "channels_enabled": s.channels_enabled,
                },
            })
        })
        .collect();
    json!({ "read_at": now_ms, "sessions": rows })
}

pub async fn run() -> Result<()> {
    marshal_entities::link();
    let client = MykoClient::new();
    let sessions = client.watch_query::<GetAllSessions>(GetAllSessions {});
    let nicknames = client.watch_query::<GetAllSessionNicknames>(GetAllSessionNicknames {});
    let watchers = client.watch_query::<GetAllWakeWatchers>(GetAllWakeWatchers {});
    client.set_address(Some(crate::daemon_address()));

    // An empty result can't be told from one that hasn't arrived, so never report
    // an empty roster as a fact.
    let started = Instant::now();
    while sessions.get().is_empty() {
        if started.elapsed() > CONNECT_TIMEOUT {
            bail!(
                "no roster within {CONNECT_TIMEOUT:?}: the daemon is unreachable or has no sessions"
            );
        }
        tokio::time::sleep(POLL).await;
    }
    let settling = Instant::now();
    while watchers.get().is_empty() && settling.elapsed() < WATCHERS_SETTLE {
        tokio::time::sleep(POLL).await;
    }

    let output = view(
        &sessions.get(),
        &nicknames.get(),
        &watchers.get(),
        Utc::now().timestamp_millis(),
    );
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use marshal_entities::{HostInfo, SessionId, WakeWatcherId};

    fn session() -> Arc<Session> {
        Arc::new(Session {
            id: SessionId(Arc::from("a")),
            client_id: None,
            pid: 7,
            cwd: "/w".into(),
            git_branch: None,
            current_task: Some("anything at all".into()),
            session_name: None,
            activity: Some("idle".into()),
            kind: None,
            connected_at: 1,
            last_activity_at: Some(2),
            last_tool: None,
            last_tool_at: None,
            operator: Some("op".into()),
            host: Some(HostInfo {
                name: "host-a".into(),
                os: String::new(),
                arch: String::new(),
            }),
            project: None,
            channels_enabled: None,
            acks_pushes: None,
            agent_id: None,
        })
    }

    #[test]
    fn claims_are_kept_apart_from_the_daemons_record() {
        let v = view(&[session()], &[], &[], 100);
        let row = &v["sessions"][0];
        assert_eq!(v["read_at"], 100);
        assert_eq!(row["declared"]["machine"], "host-a");
        assert!(row["recorded"].get("machine").is_none());
        assert!(row["recorded"].get("operator").is_none());
        assert_eq!(row["declared"]["current_task"], "anything at all");
        assert_eq!(row["declared"]["last_activity_at"], 2);
        assert!(row["recorded"].get("current_task").is_none());
        assert!(row["recorded"].get("last_activity_at").is_none());
    }

    #[test]
    fn waiting_comes_from_a_current_watcher() {
        let watcher = |until| {
            Arc::new(WakeWatcher {
                id: WakeWatcherId(Arc::from("a")),
                until,
            })
        };
        let v = view(&[session()], &[], &[watcher(101)], 100);
        assert_eq!(v["sessions"][0]["wake"], "waiting");
        assert_eq!(v["sessions"][0]["recorded"]["waiting_until"], 101);
        let v = view(&[session()], &[], &[watcher(100)], 100);
        assert_eq!(v["sessions"][0]["wake"], "none");
        assert_eq!(v["sessions"][0]["recorded"]["waiting"], false);
    }
}
