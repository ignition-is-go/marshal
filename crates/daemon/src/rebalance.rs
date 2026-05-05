//! Global role solver.
//!
//! Computes the desired role for every live session and emits a
//! `Session` SET for each one whose current role disagrees. Steady-state
//! rules:
//!
//! - Exactly one **communicator** — the most-senior session globally
//!   (smallest `connected_at`, with `session_id` as the deterministic
//!   tiebreaker).
//! - One **task_distributor** per cwd, *unless* the communicator already
//!   lives in that cwd (in which case the communicator covers that role
//!   for the folder and no separate distributor is assigned). Picked the
//!   same way: most-senior in the cwd.
//! - Every other session is a **worker**.
//!
//! "Most senior" is purely about wall-clock connection age — the rule does
//! not care whether the session is currently bound to a live client.
//! Disconnected-but-still-on-the-roster sessions (waiting for the cleanup
//! sweeper) are valid candidates; if they end up classified, the next
//! sweep that DELs them will trigger another rebalance.
//!
//! Loop termination: `execute` only emits SETs for sessions whose role
//! actually changes. The follow-up SETs trigger any SET-watching sagas
//! once, those sagas (e.g. `RoleChangeNotifySaga`) push notifications,
//! and the system converges.

use std::sync::Arc;

use entities::{GetAllSessions, Session};
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
};

/// Recompute every session's role and persist the diffs.
#[myko_command]
pub struct RebalanceRoles {}

impl CommandHandler for RebalanceRoles {
    fn execute(self, ctx: CommandContext) -> Result<(), CommandError> {
        let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;
        let assignments = compute_assignments(&sessions);

        let mut changes = 0usize;
        for (session, desired_role) in assignments {
            let current = session.role.as_deref();
            if current == Some(desired_role) {
                continue;
            }
            log::info!(
                "[rebalance] {} ({}): {} → {desired_role}",
                session.id.0,
                session.cwd,
                current.unwrap_or("—"),
            );
            let updated = Session {
                role: Some(desired_role.into()),
                ..(**session).clone()
            };
            ctx.emit_set(&updated)?;
            changes += 1;
        }

        if changes > 0 {
            log::info!("[rebalance] applied {changes} role change(s)");
        }
        Ok(())
    }
}

/// Compute desired role for every session in `sessions`. Returns one
/// (session, role) tuple per input session in input order.
fn compute_assignments<'a>(
    sessions: &'a [Arc<Session>],
) -> Vec<(&'a Arc<Session>, &'static str)> {
    if sessions.is_empty() {
        return Vec::new();
    }

    // Most-senior globally → communicator.
    let communicator = sessions
        .iter()
        .min_by(|a, b| {
            a.connected_at
                .cmp(&b.connected_at)
                .then_with(|| a.id.0.as_ref().cmp(b.id.0.as_ref()))
        })
        .expect("non-empty checked above");
    let communicator_cwd = communicator.cwd.clone();

    // Per cwd (excluding the communicator's), find the most-senior
    // session — that one is the task_distributor for the folder.
    use std::collections::{HashMap, HashSet};
    let mut distributor_ids: HashSet<Arc<str>> = HashSet::new();
    let mut by_cwd: HashMap<&str, Vec<&Arc<Session>>> = HashMap::new();
    for s in sessions {
        if s.cwd == communicator_cwd {
            continue;
        }
        by_cwd.entry(s.cwd.as_str()).or_default().push(s);
    }
    for (_cwd, group) in by_cwd {
        if let Some(senior) = group.into_iter().min_by(|a, b| {
            a.connected_at
                .cmp(&b.connected_at)
                .then_with(|| a.id.0.as_ref().cmp(b.id.0.as_ref()))
        }) {
            distributor_ids.insert(senior.id.0.clone());
        }
    }

    sessions
        .iter()
        .map(|s| {
            let role = if Arc::ptr_eq(s, communicator) {
                "communicator"
            } else if distributor_ids.contains(&s.id.0) {
                "task_distributor"
            } else {
                "worker"
            };
            (s, role)
        })
        .collect()
}
