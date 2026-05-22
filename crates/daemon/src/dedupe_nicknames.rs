//! Daemon-side reactive nickname disambiguation.
//!
//! Two live sessions must never share a `Session.nickname`. Shims send
//! their cwd basename verbatim (e.g. two clones of `marshal` both
//! announce `nickname = "marshal"`); the daemon converges every live
//! session to its **seniority rank** within the cwd-basename root —
//! the oldest holds the bare root, the next gets `{root}-2`, then
//! `{root}-3`, and so on.
//!
//! Seniority is `connected_at` (with `session_id` as a deterministic
//! tiebreaker). A session's seniority stays fixed for its whole life,
//! even across daemon restarts (because `connected_at` is persisted),
//! so a long-running session keeps its name when transient peers come
//! and go.
//!
//! Wiring:
//! - `DedupeNicknameSetSaga` — fires on every `Session` SET. Cheap peek:
//!   only dispatches `DedupeNicknames` when the newly-set row's nickname
//!   collides with another live session's. The steady-state no-op
//!   answer is what makes the saga loop terminate.
//! - `DedupeNicknameDelSaga` — fires on every `Session` DEL. Always
//!   dispatches `DedupeNicknames` because a departed session may have
//!   freed the bare root, letting a suffixed sibling get promoted.
//!   The command itself is the source of truth: it inspects current
//!   live state and emits a correction only if the seniority ordering
//!   is out of canonical form.
//! - `DedupeNicknames` — server-internal command that snapshots all
//!   sessions, groups them by cwd-basename root, and emits a single
//!   corrective `Session` SET against the first member whose actual
//!   nickname doesn't match its seniority-derived expected form.
//!   The corrective SET re-fires the SET saga, which dispatches
//!   another `DedupeNicknames` if more work remains. Convergence in
//!   one extra round trip per correction; demote + promote each cost
//!   one round trip.
//!
//! The pure name-finder `dedupe_nickname` is exposed for unit-testing
//! and for any future caller that wants to predict its post-dedupe name
//! (e.g. a shim doing optimistic UI). The function strips a trailing
//! `-{digits}` suffix from `desired` so a session arriving with
//! `marshal-3` whose true root is `marshal` still re-converges from the
//! root rather than getting compounded into `marshal-3-2`.
//!
//! Transient duplicates: during a promote+demote rebalance, the saga
//! issues two corrective SETs serially (no atomic batch API). For ~ms
//! between them, two sessions may share the same nickname. This window
//! is acceptable per the same "single SET per pass" discipline that
//! existed before rebalancing was added.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

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
pub struct DedupeNicknameSetSaga;

impl SagaHandler for DedupeNicknameSetSaga {
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

/// Fires on every `Session` DEL. Unconditionally dispatches
/// `DedupeNicknames` because a departed session may have freed the bare
/// root, letting a suffixed sibling get promoted. The command itself
/// returns early when no correction is needed, so the steady-state cost
/// is one `GetAllSessions` query per DEL.
#[myko_saga]
pub struct DedupeNicknameDelSaga;

impl SagaHandler for DedupeNicknameDelSaga {
    type EventItem = Session;
    type Command = DedupeNicknames;
    const EVENT_TYPE: MEventType = MEventType::DEL;

    fn handle(_session: Session, _event: MEvent, _ctx: Arc<SagaContext>) -> Option<Self::Command> {
        Some(DedupeNicknames {})
    }
}

// ─── Command ────────────────────────────────────────────────────────────────

/// Snapshot every live session, find the first one whose nickname
/// disagrees with its seniority-derived canonical form, and emit a
/// single corrective `Session` SET to fix it.
///
/// "Single SET per pass" is intentional: the corrective SET re-fires
/// `DedupeNicknameSetSaga`, which dispatches another `DedupeNicknames`
/// if more work remains. This walks the rebalance one fix at a time;
/// see the module docstring for the transient-duplicate window during
/// a promote+demote pair.
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

/// Pick at most one session whose actual nickname disagrees with its
/// seniority-derived expected form. Returns `None` when every live
/// session is already in canonical order.
///
/// Canonical form: group live sessions by their cwd-basename root
/// (recovered by stripping any trailing `-{digits}` suffix from the
/// post-dedupe nickname). Within each group sort by seniority
/// (`connected_at`, then `session_id` as a deterministic tiebreaker).
/// The 0th member should hold the bare root, the 1st should hold
/// `{root}-2`, the 2nd `{root}-3`, and so on.
///
/// This handles both directions of correction in one pass:
/// - **Demote** — a newly-arrived session whose nickname duplicates
///   an older sibling's gets bumped to the next suffix.
/// - **Promote** — when a session DELs, every surviving sibling
///   shifts one rank towards the bare root; the next saga iteration
///   emits the corresponding SET.
///
/// Returning only one correction per call keeps every emitted SET
/// individually well-formed (one nickname assignment at a time, in a
/// known direction). The caller drives convergence by re-firing.
///
/// Group iteration is in BTreeMap (lexicographic root) order so the
/// pick is deterministic when multiple groups need fixing, mirroring
/// the pre-existing collision-only behaviour.
fn pick_one_correction(sessions: &[Arc<Session>]) -> Option<(&Arc<Session>, String)> {
    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, s) in sessions.iter().enumerate() {
        let root = strip_dash_digits(s.nickname.as_str());
        groups.entry(root).or_default().push(i);
    }

    for (root, mut indices) in groups {
        indices.sort_by(|&a, &b| {
            sessions[a]
                .connected_at
                .cmp(&sessions[b].connected_at)
                .then_with(|| sessions[a].id.0.as_ref().cmp(sessions[b].id.0.as_ref()))
        });
        for (rank, &i) in indices.iter().enumerate() {
            let expected = if rank == 0 {
                root.to_string()
            } else {
                format!("{root}-{}", rank + 1)
            };
            if sessions[i].nickname != expected {
                return Some((&sessions[i], expected));
            }
        }
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

    // ─── pick_one_correction (rebalance) tests ─────────────────────────────

    use marshal_entities::SessionId;

    /// Minimal Session builder for correction-pass tests. Only the
    /// fields `pick_one_correction` reads are meaningful; the rest get
    /// stable defaults.
    fn s(id: &'static str, nick: &str, connected_at: i64) -> Arc<Session> {
        Arc::new(Session {
            id: SessionId(Arc::from(id)),
            client_id: None,
            nickname: nick.to_string(),
            pid: 0,
            cwd: String::new(),
            git_branch: None,
            current_task: None,
            connected_at,
            last_activity_at: None,
            last_tool: None,
            last_tool_at: None,
            operator: None,
            host: None,
            project: None,
        })
    }

    fn correction(result: Option<(&Arc<Session>, String)>) -> Option<(&str, String)> {
        result.map(|(s, n)| (s.id.0.as_ref(), n))
    }

    #[test]
    fn canonical_state_needs_no_correction() {
        let sessions = vec![
            s("a", "marshal", 100),
            s("b", "marshal-2", 200),
            s("c", "marshal-3", 300),
            s("d", "other", 150),
        ];
        assert_eq!(pick_one_correction(&sessions), None);
    }

    #[test]
    fn collision_demotes_younger() {
        // Both sessions arrive with `marshal`. Older keeps it, younger
        // bumps to `marshal-2`.
        let sessions = vec![s("old", "marshal", 100), s("new", "marshal", 200)];
        assert_eq!(
            correction(pick_one_correction(&sessions)),
            Some(("new", "marshal-2".to_string())),
        );
    }

    #[test]
    fn lone_suffixed_session_gets_promoted() {
        // After the original holder DEL'd, the surviving suffixed
        // session should be promoted to the bare root.
        let sessions = vec![s("a", "marshal-2", 100)];
        assert_eq!(
            correction(pick_one_correction(&sessions)),
            Some(("a", "marshal".to_string())),
        );
    }

    #[test]
    fn oldest_suffixed_promotes_first_when_root_vacant() {
        // `marshal` is free; the two suffixed survivors should
        // converge to {marshal, marshal-2}, with the older taking the
        // bare root.
        let sessions = vec![s("y", "marshal-3", 200), s("x", "marshal-2", 100)];
        assert_eq!(
            correction(pick_one_correction(&sessions)),
            Some(("x", "marshal".to_string())),
        );
    }

    #[test]
    fn observed_pulse_deploy_case_corrects_older_session() {
        // Real-world scenario from the marshal-01 event log: an older
        // session got demoted to `pulse-deploy-2` when a peer briefly
        // held `pulse-deploy`, then that peer DEL'd, and a newer
        // session claimed the bare name. The older session was left
        // stranded at `-2`. Rebalance must promote it back.
        let sessions = vec![
            s("older", "pulse-deploy-2", 1779397537134),
            s("newer", "pulse-deploy", 1779404776725),
        ];
        // First pass: older session is the rank-0 member of the group,
        // so its expected name is the bare root.
        assert_eq!(
            correction(pick_one_correction(&sessions)),
            Some(("older", "pulse-deploy".to_string())),
        );
    }

    #[test]
    fn rebalance_converges_in_finite_passes() {
        // Drive the correction pass to a fixed point manually so we
        // exercise the full demote→promote sequence in test isolation.
        let initial = vec![
            s("older", "pulse-deploy-2", 100),
            s("newer", "pulse-deploy", 200),
        ];
        let mut sessions = initial;
        for _ in 0..8 {
            let Some((victim, new_name)) = pick_one_correction(&sessions) else {
                break;
            };
            let victim_id = victim.id.0.clone();
            let new_name = new_name.clone();
            sessions = sessions
                .into_iter()
                .map(|s| {
                    if s.id.0 == victim_id {
                        Arc::new(Session {
                            nickname: new_name.clone(),
                            ..(*s).clone()
                        })
                    } else {
                        s
                    }
                })
                .collect();
        }
        assert_eq!(pick_one_correction(&sessions), None);
        let by_id: std::collections::HashMap<&str, &str> = sessions
            .iter()
            .map(|s| (s.id.0.as_ref(), s.nickname.as_str()))
            .collect();
        assert_eq!(by_id["older"], "pulse-deploy");
        assert_eq!(by_id["newer"], "pulse-deploy-2");
    }

    #[test]
    fn deterministic_pick_across_groups() {
        // Two groups, each needing one correction. The pick must be
        // stable run-to-run — BTreeMap (lexicographic) walk guarantees
        // we always see `alpha` before `beta`.
        let sessions = vec![
            s("a1", "alpha", 100),
            s("a2", "alpha", 200),
            s("b1", "beta", 100),
            s("b2", "beta", 200),
        ];
        // `alpha` group goes first; younger (`a2`) gets demoted.
        assert_eq!(
            correction(pick_one_correction(&sessions)),
            Some(("a2", "alpha-2".to_string())),
        );
    }
}
