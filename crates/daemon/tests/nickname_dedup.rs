//! Integration tests for the nickname-dedup saga.
//!
//! Spins up a real `CellServer` against a temp `events.jsonl` (no WS
//! listener), applies synthetic `Session` SETs via `apply_event_batch`,
//! then drives the dedup pass to convergence using
//! `dedupe_nicknames::run_until_converged` — the same correction the
//! saga runtime performs in production, just stepped synchronously for
//! test determinism.
//!
//! The convergence helper bounds itself to a fixed pass count and
//! returns `Err` if it didn't reach a steady state in time. That
//! doubles as a guard against the saga's idempotency invariant
//! regressing into an infinite re-SET loop.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use daemon::{
    dedupe_nicknames::{DedupeNicknameDelSaga, run_until_converged},
    persister::DiskPersister,
};
use hyphae::Gettable;
use marshal_entities::{Session, SessionId};
use myko::{
    core::item::Eventable,
    saga::{SagaContext, SagaHandler},
    server::{CellServerCtx, Persister},
    utils::downcast_item,
    wire::{MEvent, MEventType},
};
use myko_server::CellServer;

/// Generous pass cap. The dedup converges in ≤N rounds for N colliding
/// sessions in real life; using 32 here gives the test plenty of slack
/// while still failing fast if a regression ever sends us into a loop.
const MAX_PASSES: usize = 32;

fn setup() -> (
    PathBuf,
    Arc<DiskPersister>,
    CellServerCtx,
    tempfile::TempDir,
) {
    marshal_entities::link();
    daemon::link();

    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("events.jsonl");
    let persister = Arc::new(DiskPersister::new(&log_path).expect("open log"));

    let server = CellServer::builder()
        .with_default_persister(persister.clone() as Arc<dyn Persister>)
        .build();
    let ctx = server.ctx();
    persister.replay(&ctx).expect("replay empty log");

    let server: &'static CellServer = Box::leak(Box::new(server));
    let _ = server;

    (log_path, persister, ctx, dir)
}

/// Build a Session entity with the given id + nickname; everything else
/// is filler.
fn session(id: &str, nickname: &str, connected_at: i64) -> Session {
    Session {
        id: SessionId(Arc::from(id)),
        client_id: None,
        nickname: nickname.into(),
        pid: 0,
        cwd: "/repo".into(),
        git_branch: None,
        current_task: None,
        connected_at,
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
        operator: None,
        host: None,
        project: None,
    }
}

fn set_session(ctx: &CellServerCtx, s: &Session) {
    let event = MEvent::from_item(s, MEventType::SET, &uuid::Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event])
        .expect("apply Session SET");
}

fn del_session(ctx: &CellServerCtx, s: &Session) {
    let event = MEvent::from_item(s, MEventType::DEL, &uuid::Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event])
        .expect("apply Session DEL");
}

/// Snapshot session-id → nickname for every Session row in the registry.
fn nicknames_by_id(ctx: &CellServerCtx) -> HashMap<String, String> {
    let Some(store) = ctx.registry.get(Session::ENTITY_NAME_STATIC) else {
        return Default::default();
    };
    store
        .entries()
        .get()
        .into_iter()
        .filter_map(|(id, item)| {
            let s: Session = downcast_item(&item)?;
            Some((id.as_ref().to_string(), s.nickname))
        })
        .collect()
}

/// Inverse view: nickname → session_id (panics on duplicates so the
/// dedup invariant is exercised at every assertion site).
fn unique_nicknames(ctx: &CellServerCtx) -> HashMap<String, String> {
    let map = nicknames_by_id(ctx);
    let mut out: HashMap<String, String> = HashMap::new();
    for (id, nick) in map {
        if let Some(prev) = out.insert(nick.clone(), id.clone()) {
            panic!(
                "duplicate nickname '{nick}' on sessions {prev} and {id} \
                 — dedup invariant violated"
            );
        }
    }
    out
}

#[test]
fn two_sessions_with_same_nickname_get_disambiguated() {
    let (_log, _p, ctx, _dir) = setup();
    set_session(&ctx, &session("a", "marshal", 100));
    set_session(&ctx, &session("b", "marshal", 200));

    let applied = run_until_converged(&ctx, MAX_PASSES).expect("converged");
    // Exactly one corrective SET — only the second session needed a
    // suffix. (The first kept its bare name.)
    assert_eq!(applied, 1, "expected one corrective SET");

    let by_nick = unique_nicknames(&ctx);
    assert!(
        by_nick.contains_key("marshal"),
        "one session should still hold the bare 'marshal'",
    );
    assert!(
        by_nick.contains_key("marshal-2"),
        "the other session should be deduped to 'marshal-2'; got {by_nick:?}",
    );
    assert_eq!(
        by_nick.len(),
        2,
        "exactly two sessions, both with unique names"
    );
}

#[test]
fn three_collisions_walk_to_marshal_3() {
    let (_log, _p, ctx, _dir) = setup();
    set_session(&ctx, &session("a", "marshal", 100));
    set_session(&ctx, &session("b", "marshal", 200));
    set_session(&ctx, &session("c", "marshal", 300));

    let applied = run_until_converged(&ctx, MAX_PASSES).expect("converged");
    assert_eq!(applied, 2, "two collisions → two corrective SETs");

    let by_nick = unique_nicknames(&ctx);
    assert!(by_nick.contains_key("marshal"));
    assert!(by_nick.contains_key("marshal-2"));
    assert!(by_nick.contains_key("marshal-3"));
    assert_eq!(by_nick.len(), 3);
}

#[test]
fn deleting_a_deduped_row_promotes_the_survivor_and_demotes_the_newcomer() {
    let (_log, _p, ctx, _dir) = setup();
    // Three colliders converge to marshal / -2 / -3.
    set_session(&ctx, &session("a", "marshal", 100));
    set_session(&ctx, &session("b", "marshal", 200));
    set_session(&ctx, &session("c", "marshal", 300));
    run_until_converged(&ctx, MAX_PASSES).expect("initial convergence");

    // Identify whichever session ended up at "marshal-2" and DEL it.
    let before = unique_nicknames(&ctx);
    let marshal_2_owner = before
        .get("marshal-2")
        .expect("marshal-2 should be held after initial dedup")
        .clone();
    let to_del = session(&marshal_2_owner, "marshal-2", 0);
    del_session(&ctx, &to_del);
    assert!(
        !unique_nicknames(&ctx).contains_key("marshal-2"),
        "DEL should free the marshal-2 slot",
    );

    // The DEL fires `DedupeNicknameDelSaga`, which promotes the
    // suffixed survivor (c, "marshal-3") into the now-vacant
    // "marshal-2" slot — canonical seniority order says the next-
    // oldest in the group gets the next-smallest suffix.
    run_until_converged(&ctx, MAX_PASSES).expect("rebalance after DEL");
    let post_del = unique_nicknames(&ctx);
    assert_eq!(
        post_del.get("marshal-2").map(String::as_str),
        Some("c"),
        "c should be promoted from marshal-3 into the freed marshal-2 slot",
    );
    assert!(
        !post_del.contains_key("marshal-3"),
        "marshal-3 should now be free; got {post_del:?}",
    );

    // A fresh colliding session arrives. Its expected rank is the new
    // tail of the group (3rd, since a and c hold ranks 0 and 1), so it
    // converges to "marshal-3".
    set_session(&ctx, &session("d", "marshal", 400));
    run_until_converged(&ctx, MAX_PASSES).expect("converged after re-add");

    let after = unique_nicknames(&ctx);
    assert!(after.contains_key("marshal"));
    assert!(after.contains_key("marshal-2"));
    assert!(after.contains_key("marshal-3"));
    assert_eq!(after.len(), 3);
    assert_eq!(
        after.get("marshal-3").map(String::as_str),
        Some("d"),
        "the new arrival is the youngest in the group → takes marshal-3",
    );
}

#[test]
fn rename_to_a_free_name_is_accepted_as_is() {
    // Spec test 4 / "easier test": Session X is sitting at "marshal-2"
    // (e.g. from an earlier dedup) and re-SETs itself with the bare
    // nickname "marshal". If no other session holds "marshal", this is
    // a legitimate rename — the saga snapshots OTHER sessions, sees no
    // collision, and the bare name is accepted as-is.
    let (_log, _p, ctx, _dir) = setup();
    set_session(&ctx, &session("x", "marshal-2", 100));

    // Re-SET with the bare name. No other session is named "marshal".
    set_session(&ctx, &session("x", "marshal", 100));
    let applied = run_until_converged(&ctx, MAX_PASSES).expect("converged");
    assert_eq!(applied, 0, "no collision → no corrective SET");

    let by_nick = unique_nicknames(&ctx);
    assert_eq!(by_nick.len(), 1);
    assert!(
        by_nick.contains_key("marshal"),
        "rename to free name should be accepted; got {by_nick:?}",
    );
}

#[test]
fn convergence_terminates_under_the_pass_cap() {
    // Pin the idempotency invariant at the integration level: a steady
    // state with three already-unique sessions must produce zero
    // additional corrections, and the convergence helper must return
    // immediately rather than walking up to MAX_PASSES.
    let (_log, _p, ctx, _dir) = setup();
    set_session(&ctx, &session("a", "alpha", 100));
    set_session(&ctx, &session("b", "bravo", 200));
    set_session(&ctx, &session("c", "charlie", 300));
    let applied = run_until_converged(&ctx, MAX_PASSES).expect("converged");
    assert_eq!(applied, 0, "no collisions → no work");
}

#[test]
fn del_saga_handler_unconditionally_dispatches_dedupe() {
    // The DEL saga is no-state-peek by design: every Session DEL fires
    // a DedupeNicknames pass, and the command itself short-circuits when
    // no correction is needed. Pin that the handler returns Some on a
    // DEL event so a future refactor can't silently introduce a peek
    // that would skip rebalances after a vacancy.
    //
    // The macro-driven saga *registration* (firing on the right
    // MEventType) is exercised by the existing SET-saga path in
    // production usage; this test pins only the handler behaviour.
    let (_log, _p, ctx, _dir) = setup();

    let s = session("any", "marshal", 100);
    let event = MEvent::from_item(&s, MEventType::DEL, "tx-id");
    let saga_ctx = Arc::new(SagaContext::new(
        uuid::Uuid::new_v4(),
        ctx.registry.clone(),
    ));

    let result = DedupeNicknameDelSaga::handle(s, event, saga_ctx);
    assert!(
        result.is_some(),
        "DEL saga must dispatch DedupeNicknames on every Session DEL",
    );
}
