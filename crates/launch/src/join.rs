//! Waiting for a launched agent to join the roster, and the join proof.
//!
//! `agent_id` is self-asserted, so any session could present a launch's
//! `launch-<uuid>`. A row counts as the launched agent only if it also runs on
//! the host the adapter reported and in the folder it reported. That isn't
//! authentication, which upstream's trust model leaves to the mesh; it keeps a
//! stray or mistaken session from being taken for the launched one.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use hyphae::Gettable;
use marshal_entities::{GetAllSessionNicknames, GetAllSessions, Session, SessionNickname};
use myko::client::{ConnectionStatus, MykoClient};

use crate::Launched;

/// How long to wait for the launcher's own connection to the daemon.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(200);

/// What the roster says about a launch.
#[derive(Debug, PartialEq)]
pub enum Join<'a> {
    /// A row carries the launch's id, on the reported host and in the reported folder.
    Joined(&'a Session),
    /// No row carries the launch's id yet. The strings say why any row that
    /// carried it was refused.
    Waiting(Vec<String>),
}

/// Whether two host names are the same machine. The shim reports the full
/// `gethostname` from its own registration but only the first label from its
/// hooks (`build-03.mesh` vs `build-03`), so names are compared by first label,
/// ignoring case.
pub fn same_host(a: &str, b: &str) -> bool {
    let first = |h: &str| {
        h.trim()
            .split('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    let (a, b) = (first(a), first(b));
    !a.is_empty() && a == b
}

fn same_dir(a: &str, b: &str) -> bool {
    let trim = |s: &str| {
        let t = s.trim_end_matches('/');
        if t.is_empty() { "/" } else { t }.to_string()
    };
    trim(a) == trim(b)
}

/// Find the launched agent's row: `agent_id`, plus `host.name` and `cwd` from the adapter's report.
pub fn find<'a>(sessions: &'a [Arc<Session>], agent_id: &str, launched: &Launched) -> Join<'a> {
    let mut refused = Vec::new();
    let mut matching: Option<&Session> = None;
    for s in sessions.iter().map(|s| s.as_ref()) {
        if s.agent_id.as_deref() != Some(agent_id) {
            continue;
        }
        let host = s.host.as_ref().map(|h| h.name.as_str());
        if !host.is_some_and(|h| same_host(h, &launched.host)) {
            refused.push(format!(
                "session {} carries this launch's id but runs on {}, not {}",
                s.id.0,
                host.unwrap_or("an unreported host"),
                launched.host
            ));
            continue;
        }
        if !same_dir(&s.cwd, &launched.cwd) {
            refused.push(format!(
                "session {} carries this launch's id but runs in {}, not {}",
                s.id.0, s.cwd, launched.cwd
            ));
            continue;
        }
        if let Some(first) = matching {
            refused.push(format!(
                "sessions {} and {} both match this launch; no session was selected",
                first.id.0, s.id.0
            ));
            return Join::Waiting(refused);
        }
        matching = Some(s);
    }
    matching.map_or(Join::Waiting(refused), Join::Joined)
}

/// The daemon-assigned nickname for `s`, or the computed one before it's assigned.
pub fn nickname_of(nicknames: &[Arc<SessionNickname>], s: &Session) -> String {
    nicknames
        .iter()
        .find(|n| n.id.0 == s.id.0)
        .map(|n| n.nickname.clone())
        .unwrap_or_else(|| marshal_entities::nickname(&s.id.0))
}

/// The launcher's view of the roster.
pub struct Roster {
    client: MykoClient,
    address: String,
    sessions: hyphae::Cell<Vec<Arc<Session>>, hyphae::CellImmutable>,
    nicknames: hyphae::Cell<Vec<Arc<SessionNickname>>, hyphae::CellImmutable>,
}

/// A launched agent that joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Joined {
    pub session_id: String,
    pub nickname: String,
}

impl Roster {
    /// Connect to the daemon at `address`, or say it couldn't, before anything starts.
    pub async fn connect(address: &str) -> Result<Self> {
        marshal_entities::link();
        let client = MykoClient::new();
        let sessions = client.watch_query::<GetAllSessions>(GetAllSessions {});
        let nicknames = client.watch_query::<GetAllSessionNicknames>(GetAllSessionNicknames {});
        client.set_address(Some(address.to_string()));
        let started = Instant::now();
        while !matches!(
            client.connection_status_sync(),
            ConnectionStatus::Connected(_)
        ) {
            if started.elapsed() > CONNECT_TIMEOUT {
                bail!(
                    "couldn't reach the Marshal daemon at {address} within {}s, so nothing was started",
                    CONNECT_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(POLL).await;
        }
        Ok(Self {
            client,
            address: address.to_string(),
            sessions,
            nicknames,
        })
    }

    pub fn client(&self) -> &MykoClient {
        &self.client
    }

    /// The roster as last seen.
    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.sessions.get()
    }

    /// Wait for the launched agent's row. Backend status is independent of this deadline.
    pub async fn wait(
        &self,
        agent_id: &str,
        launched: &Launched,
        timeout: Duration,
    ) -> Result<Joined> {
        let started = Instant::now();
        loop {
            let sessions = self.sessions.get();
            let refused = match find(&sessions, agent_id, launched) {
                Join::Joined(s) => {
                    return Ok(Joined {
                        session_id: s.id.0.to_string(),
                        nickname: nickname_of(&self.nicknames.get(), s),
                    });
                }
                Join::Waiting(refused) => refused,
            };
            if started.elapsed() > timeout {
                let mut msg = format!(
                    "no session carrying {agent_id} joined the roster at {} from {} in {} within {}s.",
                    self.address,
                    launched.host,
                    launched.cwd,
                    timeout.as_secs()
                );
                for r in &refused {
                    msg.push_str(&format!(" Refused: {r}."));
                }
                msg.push_str(&format!(
                    " It may still be running at {} (a first-run prompt, such as trusting the folder, \
                     holds a session back until it's answered).",
                    launched.r#where
                ));
                if let Some(attach) = &launched.attach {
                    msg.push_str(&format!(" See it with `{attach}`."));
                }
                msg.push_str(&format!(
                    " Stop it with `marshal-launch stop {}`.",
                    agent_id
                ));
                bail!(msg);
            }
            tokio::time::sleep(POLL.min(timeout.saturating_sub(started.elapsed()))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marshal_entities::{HostInfo, SessionId};

    fn row(id: &str, host: Option<&str>, cwd: &str, agent_id: Option<&str>) -> Arc<Session> {
        let mut v = serde_json::json!({
            "id": id,
            "pid": 1,
            "cwd": cwd,
            "connectedAt": 1,
        });
        if let Some(a) = agent_id {
            v["agentId"] = a.into();
        }
        let mut s: Session = serde_json::from_value(v).unwrap();
        s.host = host.map(|h| HostInfo {
            name: h.into(),
            os: "linux".into(),
            arch: "x86_64".into(),
        });
        assert_eq!(s.id, SessionId(Arc::from(id)));
        Arc::new(s)
    }

    fn launched() -> Launched {
        Launched {
            handle: "h".into(),
            host: "build-03".into(),
            cwd: "/w/app".into(),
            r#where: "build-03 tmux:h".into(),
            attach: None,
        }
    }

    #[test]
    fn the_row_with_the_id_on_the_host_in_the_folder_has_joined() {
        let rows = [
            row("other", Some("build-03"), "/w/app", None),
            row("mine", Some("build-03"), "/w/app/", Some("launch-1")),
        ];
        match find(&rows, "launch-1", &launched()) {
            Join::Joined(s) => assert_eq!(s.id.0.as_ref(), "mine"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn matching_rows_do_not_select_an_arbitrary_recipient() {
        let rows = [
            row("other-thread", Some("build-03"), "/w/app", Some("launch-1")),
            row("first-thread", Some("build-03"), "/w/app", Some("launch-1")),
        ];
        let Join::Waiting(reasons) = find(&rows, "launch-1", &launched()) else {
            panic!("ambiguous launch selected an arbitrary thread");
        };
        assert!(reasons.iter().any(|reason| reason.contains("both match")));
    }

    #[test]
    fn hosts_match_by_first_label_as_the_hooks_report_them() {
        assert!(same_host("build-03.mesh", "build-03"));
        assert!(same_host("Build-03", "build-03.mesh.example"));
        assert!(!same_host("build-04", "build-03"));
        assert!(!same_host("", ""));
        let rows = [row("short", Some("build-03"), "/w/app", Some("launch-1"))];
        let mut l = launched();
        l.host = "build-03.mesh".into();
        assert!(matches!(find(&rows, "launch-1", &l), Join::Joined(_)));
    }

    #[test]
    fn the_right_id_from_another_host_or_folder_is_refused() {
        let rows = [
            row("elsewhere", Some("laptop"), "/w/app", Some("launch-1")),
            row("wrong-dir", Some("build-03"), "/w/other", Some("launch-1")),
            row("no-host", None, "/w/app", Some("launch-1")),
            row("other-launch", Some("build-03"), "/w/app", Some("launch-2")),
        ];
        match find(&rows, "launch-1", &launched()) {
            Join::Waiting(refused) => {
                assert_eq!(refused.len(), 3, "{refused:?}");
                assert!(refused[0].contains("laptop"));
                assert!(refused[1].contains("/w/other"));
                assert!(refused[2].contains("unreported host"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(find(&[], "launch-1", &launched()), Join::Waiting(vec![]));
    }
}
