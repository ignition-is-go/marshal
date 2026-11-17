//! The first task: one direct message to the joined session, sent under a
//! short-lived session of kind `launcher`.
//!
//! Sends go through Myko like any other, so the sender must be on the roster.
//! The launcher registers its own row under a fresh id, sends, and removes the
//! row. Its body says who launched the agent, so the task never looks as if it
//! came from the agent's own tools. This is the only message `marshal-launch`
//! sends. Direct mail wakes an idle session through its hooks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hyphae::{Cell, CellImmutable, Gettable};
use marshal_entities::{
    DeleteSession, DeleteSessionResult, HostInfo, RegisterSession, SendMessage, SendMessageResult,
    Session, SessionId,
};

use crate::join::Roster;

/// The `kind` on the launcher's short-lived roster row.
pub const LAUNCHER_KIND: &str = "launcher";
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

/// The task as sent: labelled with where it came from and who started the launch.
pub fn labelled(task: &str, launcher_host: &str, operator: Option<&str>, agent_id: &str) -> String {
    let by = operator
        .filter(|o| !o.trim().is_empty())
        .map(|o| format!(" for {o}"))
        .unwrap_or_default();
    format!(
        "[first task from marshal-launch on {launcher_host}{by}, which started you as {agent_id}; \
         it can't receive replies] {task}"
    )
}

/// The launcher's row. Built from JSON so fields the entity gains later default.
pub fn launcher_session(
    id: &str,
    host: &str,
    cwd: &str,
    operator: Option<&str>,
) -> Result<Session> {
    let now = chrono_ms();
    let mut v = serde_json::json!({
        "id": id,
        "pid": std::process::id(),
        "cwd": cwd,
        "kind": LAUNCHER_KIND,
        "connectedAt": now,
        "lastActivityAt": now,
    });
    if let Some(o) = operator {
        v["operator"] = o.into();
    }
    let mut s: Session = serde_json::from_value(v).context("building the launcher's roster row")?;
    s.host = Some(HostInfo {
        name: host.into(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
    });
    Ok(s)
}

fn chrono_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

async fn await_command<R>(
    cell: Cell<Option<Result<R, String>>, CellImmutable>,
    timeout: Duration,
) -> Result<R, String>
where
    R: Clone + std::fmt::Debug + PartialEq + Send + Sync + 'static,
{
    let started = Instant::now();
    loop {
        if let Some(r) = cell.get() {
            return r;
        }
        if started.elapsed() > timeout {
            return Err(format!(
                "no answer from the daemon within {}s",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Send `body` to `to` under a fresh `launcher` row, and remove the row after.
/// Returns the message id.
pub async fn send(
    roster: &Roster,
    to: &str,
    body: String,
    host: &str,
    operator: Option<&str>,
) -> Result<String> {
    let client = roster.client();
    let id = uuid::Uuid::new_v4().to_string();
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let session = launcher_session(&id, host, &cwd, operator)?;
    let registered = client.send_command::<RegisterSession, ()>(&RegisterSession {
        session: session.clone(),
    });
    await_command(registered, REGISTER_TIMEOUT)
        .await
        .map_err(|e| anyhow!("couldn't register the launcher's session: {e}"))?;
    let result = async {
        let started = Instant::now();
        while !roster.sessions().iter().any(|s| s.id == session.id) {
            if started.elapsed() > REGISTER_TIMEOUT {
                bail!("the launcher's session didn't appear on the roster");
            }
            tokio::time::sleep(POLL).await;
        }
        let sent = client.send_command::<SendMessage, SendMessageResult>(&SendMessage {
            to_session_id: SessionId(Arc::from(to)),
            response_required: false,
            body,
            // The sender is this connection's own row, so no self-identification.
            as_session: None,
        });
        let sent = await_command(sent, SEND_TIMEOUT)
            .await
            .map_err(|e| anyhow!("the daemon didn't take the first task: {e}"))?;
        Ok(sent.message_id.0.to_string())
    }
    .await;
    let deleted = client.send_command::<DeleteSession, DeleteSessionResult>(&DeleteSession {
        id: session.id.clone(),
    });
    let _ = await_command(deleted, REGISTER_TIMEOUT).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_says_who_launched_the_agent() {
        let b = labelled("say ready", "build-03", Some("op@example"), "launch-1");
        assert!(b.starts_with("[first task from marshal-launch on build-03 for op@example"));
        assert!(b.contains("launch-1") && b.ends_with("] say ready"), "{b}");
        let b = labelled("x", "build-03", Some(" "), "launch-1");
        assert!(!b.contains(" for "), "{b}");
    }

    #[test]
    fn the_launchers_row_is_kind_launcher_with_its_host() {
        let s = launcher_session("id-1", "build-03", "/w", Some("op")).unwrap();
        assert_eq!(s.kind.as_deref(), Some(LAUNCHER_KIND));
        assert_eq!(s.host.unwrap().name, "build-03");
        assert_eq!(s.operator.as_deref(), Some("op"));
        assert_eq!(s.agent_id, None);
        assert_eq!(s.client_id, None);
    }
}
