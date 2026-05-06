//! Daemon-side reactive nickname disambiguation.
//!
//! Two live sessions must never share a `Session.nickname`. Shims send
//! their cwd basename verbatim (e.g. two clones of `marshal` both
//! announce `nickname = "marshal"`); the daemon corrects collisions by
//! appending the smallest integer suffix `>= 2` (`marshal` → `marshal-2`,
//! `marshal-3`, ...).
//!
//! Wiring:
//! - `DedupeNicknameSaga` — fires on every `Session` SET. If the post-SET
//!   row's nickname collides with another live session's nickname,
//!   returns a `DedupeNicknames` command. Returns `None` otherwise (the
//!   common steady-state case — this is what makes the loop terminate).
//! - `DedupeNicknames` — server-internal command that snapshots all
//!   sessions, finds the first one whose nickname duplicates another's,
//!   and emits a single corrective `Session` SET with a deduped name.
//!   The corrective SET re-fires the saga, which observes the now-unique
//!   name and no-ops. Convergence in one extra round trip per dedupe.
//!
//! The pure name-finder `dedupe_nickname` is exposed for unit-testing
//! and for any future caller that wants to predict its post-dedupe name
//! (e.g. a shim doing optimistic UI). The function strips a trailing
//! `-{digits}` suffix from `desired` so a session arriving with
//! `marshal-3` whose true root is `marshal` still re-converges from the
//! root rather than getting compounded into `marshal-3-2`.
//!
//! Free-name reuse on DEL is not handled here — when a session DELs its
//! nickname disappears from the live set, and the next colliding SET
//! that runs the saga will find the freed slot. There is intentionally
//! no active "shift `-3` down to `-2`" pass.

use std::{collections::HashSet, sync::Arc};

use marshal_entities::{GetAllSessions, Session};
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    core::item::Eventable,
    myko_command,
    prelude::myko_saga,
    saga::{SagaContext, SagaHandler},
    server::CellServerCtx,
    utils::downcast_item,
    wire::{MEvent, MEventType},
};
use uuid::Uuid;

/// Force-link saga registrations from this module against dead-code
/// elimination.
pub fn link() {}

// ─── Pure helper ────────────────────────────────────────────────────────────

/// Pick the smallest unique nickname for `desired` against the set of
/// already-`taken` nicknames.
///
/// Algorithm:
/// 1. Strip a trailing `-{digits}` suffix from `desired` to recover the
///    root name. So `marshal-3` re-converges against the `marshal`
///    root rather than compounding into `marshal-3-2` on collision.
/// 2. If the bare root is not in `taken`, return it.
/// 3. Otherwise walk `N = 2, 3, ...` and return the first `{root}-{N}`
///    not in `taken`.
///
/// `taken` should be the set of nicknames held by every OTHER live
/// session — never include the calling session's own current nickname,
/// or a session deduped to `marshal-2` would self-collide on a re-SET.
pub fn dedupe_nickname(desired: &str, taken: &HashSet<&str>) -> String {
    let base = strip_dash_digits(desired);
    if !taken.contains(base) {
        return base.to_string();
    }
    let mut n: u32 = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
        n += 1;
    }
}

/// If `s` ends with `-` followed by one or more ASCII digits, return the
/// prefix before the dash. Otherwise return `s` unchanged.
fn strip_dash_digits(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    // Need at least one digit AND a preceding `-`.
    if i == bytes.len() || i == 0 || bytes[i - 1] != b'-' {
        return s;
    }
    // Also reject "abc-" with no digits (already filtered: i < len).
    // Return the slice before the `-`.
    &s[..i - 1]
}

// ─── Saga ───────────────────────────────────────────────────────────────────

/// Fires on every `Session` SET. If the post-SET row collides with
/// another live session's nickname, returns `DedupeNicknames` to emit
/// a corrective SET. Idempotent — the no-collision case returns `None`,
/// which is what stops the corrective SET from re-firing forever.
#[myko_saga]
pub struct DedupeNicknameSaga;

impl SagaHandler for DedupeNicknameSaga {
    type EventItem = Session;
    type Command = DedupeNicknames;
    const EVENT_TYPE: MEventType = MEventType::SET;

    fn handle(session: Session, _event: MEvent, ctx: Arc<SagaContext>) -> Option<Self::Command> {
        let store = ctx.registry.get(Session::ENTITY_NAME_STATIC)?;
        // Build the collision set from OTHER sessions only — including
        // ourselves would make a re-SET of an already-deduped name flag
        // itself as colliding and bounce to the next suffix. We
        // materialize the downcast nicknames into owned `String`s so the
        // borrow on `snapshot` ends before the `taken` set is consulted.
        let other_nicknames: Vec<String> = store
            .snapshot()
            .into_iter()
            .filter_map(|(_, item)| downcast_item::<Session>(&item))
            .filter(|other| other.id != session.id)
            .map(|other| other.nickname)
            .collect();
        let taken: HashSet<&str> = other_nicknames.iter().map(String::as_str).collect();
        if !taken.contains(session.nickname.as_str()) {
            return None;
        }
        log::info!(
            "[dedupe-nickname] session {} nickname '{}' collides; dispatching dedupe pass",
            session.id.0,
            session.nickname,
        );
        Some(DedupeNicknames {})
    }
}

// ─── Command ────────────────────────────────────────────────────────────────

/// Snapshot every live session, find the first one whose nickname
/// duplicates another's, and emit a single corrective `Session` SET
/// with the deduped name.
///
/// "Single SET per pass" is intentional: the corrective SET re-fires
/// `DedupeNicknameSaga`, which dispatches another `DedupeNicknames` if
/// any collision remains. This walks the dedupe one fix at a time so
/// every intermediate state is well-formed (no batch with two sessions
/// claiming the same suffix simultaneously).
#[myko_command]
pub struct DedupeNicknames {}

impl CommandHandler for DedupeNicknames {
    fn execute(self, ctx: CommandContext) -> Result<(), CommandError> {
        let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;
        let Some((victim, new_name)) = pick_one_correction(&sessions) else {
            return Ok(());
        };
        log::info!(
            "[dedupe-nickname] correcting {} from '{}' → '{}'",
            victim.id.0,
            victim.nickname,
            new_name,
        );
        let updated = Session {
            nickname: new_name,
            ..(**victim).clone()
        };
        ctx.emit_set(&updated)?;
        Ok(())
    }
}

/// Pick at most one session whose nickname collides with another's, and
/// the deduped name to assign it. Returns `None` when every nickname is
/// already unique.
///
/// Ordering: walk sessions in **seniority order** — oldest `connected_at`
/// first, with `session_id` as the deterministic tiebreaker — and pick
/// the first collider observed. Older sessions get priority on the bare
/// root name; younger ones get bumped to suffixed forms.
///
/// Without this ordering the dedup walk inherits whatever non-
/// deterministic ordering `GetAllSessions` returns (HashMap iteration
/// in practice), which would produce different "first collider"
/// pickings across runs and make integration tests flaky.
fn pick_one_correction(sessions: &[Arc<Session>]) -> Option<(&Arc<Session>, String)> {
    // Index into `sessions` sorted by seniority (oldest first), with
    // session_id as the tiebreaker.
    let mut ordered: Vec<usize> = (0..sessions.len()).collect();
    ordered.sort_by(|&a, &b| {
        sessions[a]
            .connected_at
            .cmp(&sessions[b].connected_at)
            .then_with(|| sessions[a].id.0.as_ref().cmp(sessions[b].id.0.as_ref()))
    });

    let mut seen: HashSet<&str> = HashSet::new();
    for &i in &ordered {
        let s = &sessions[i];
        if seen.contains(s.nickname.as_str()) {
            // Build the "taken" set from every OTHER session's
            // nickname — same exclusion rule the saga uses.
            let taken: HashSet<&str> = sessions
                .iter()
                .filter(|other| other.id != s.id)
                .map(|other| other.nickname.as_str())
                .collect();
            let new_name = dedupe_nickname(&s.nickname, &taken);
            return Some((s, new_name));
        }
        seen.insert(s.nickname.as_str());
    }
    None
}

// ─── Test-only convergence pass ─────────────────────────────────────────────

/// Run `DedupeNicknames` repeatedly against `ctx` until the registry
/// has no more nickname collisions, or `max_passes` is hit.
///
/// Returns the number of corrections applied. Public so integration
/// tests can drive the saga's effect synchronously without spinning up
/// the full async saga runtime.
///
/// In production the saga runtime drives the same convergence
/// asynchronously: each corrective SET re-fires the saga, which
/// dispatches another `DedupeNicknames`. This helper just collapses
/// that loop into a tight in-process iteration for test determinism.
pub fn run_until_converged(ctx: &CellServerCtx, max_passes: usize) -> Result<usize, String> {
    let mut applied = 0usize;
    for _ in 0..max_passes {
        let store = ctx
            .registry
            .get(Session::ENTITY_NAME_STATIC)
            .ok_or_else(|| "Session store not registered".to_string())?;
        let snapshot = store.snapshot();
        let sessions: Vec<Arc<Session>> = snapshot
            .into_iter()
            .filter_map(|(_, item)| downcast_item::<Session>(&item).map(Arc::new))
            .collect();
        let Some((victim, new_name)) = pick_one_correction(&sessions) else {
            return Ok(applied);
        };
        let updated = Session {
            nickname: new_name,
            ..(**victim).clone()
        };
        let event = MEvent::from_item(&updated, MEventType::SET, &Uuid::new_v4().to_string());
        ctx.apply_event_batch(vec![event])
            .map_err(|e| format!("apply_event_batch: {e}"))?;
        applied += 1;
    }
    Err(format!(
        "dedupe did not converge within {max_passes} passes — likely an idempotency bug",
    ))
}

// ─── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn taken(items: &[&'static str]) -> HashSet<&'static str> {
        items.iter().copied().collect()
    }

    #[test]
    fn no_collision_returns_input_unchanged() {
        assert_eq!(dedupe_nickname("marshal", &taken(&[])), "marshal");
        assert_eq!(
            dedupe_nickname("marshal", &taken(&["other", "another"])),
            "marshal",
        );
    }

    #[test]
    fn first_collision_picks_two() {
        assert_eq!(
            dedupe_nickname("marshal", &taken(&["marshal"])),
            "marshal-2"
        );
    }

    #[test]
    fn walks_to_three_when_two_taken() {
        assert_eq!(
            dedupe_nickname("marshal", &taken(&["marshal", "marshal-2"])),
            "marshal-3",
        );
    }

    #[test]
    fn arriving_with_dash_digit_strips_to_root() {
        // Spec-pin: a session arriving as `marshal-3` whose root
        // `marshal` is taken must dedupe against the root, not against
        // `marshal-3`. So the answer is `marshal-2`, not `marshal-3-2`.
        assert_eq!(
            dedupe_nickname("marshal-3", &taken(&["marshal"])),
            "marshal-2",
        );
    }

    #[test]
    fn arriving_with_dash_digit_returns_unchanged_when_not_taken() {
        // The shape `{root}-{N}` is just a string — if it doesn't
        // collide, no rewrite. Even though `marshal` and `marshal-2`
        // are both taken, `marshal-3` itself is free, so we accept it.
        assert_eq!(
            dedupe_nickname("marshal-3", &taken(&["marshal", "marshal-2"])),
            "marshal-3",
        );
    }

    #[test]
    fn strip_dash_digits_handles_edge_cases() {
        // No suffix → unchanged.
        assert_eq!(strip_dash_digits("marshal"), "marshal");
        // Trailing dash with no digits → unchanged ("foo-" is a literal
        // name, not a deduped form).
        assert_eq!(strip_dash_digits("foo-"), "foo-");
        // Just digits with no dash → unchanged.
        assert_eq!(strip_dash_digits("12345"), "12345");
        // Multi-digit suffix.
        assert_eq!(strip_dash_digits("marshal-42"), "marshal");
        // Internal dashes preserved.
        assert_eq!(strip_dash_digits("foo-bar-7"), "foo-bar");
        // Empty.
        assert_eq!(strip_dash_digits(""), "");
    }
}
