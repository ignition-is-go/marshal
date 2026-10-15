//! Periodic sweep that DELs abandoned sessions.
//!
//! Two session kinds, two liveness rules:
//!
//! - **WS shim sessions** (`client_id: Some`): live while that client id
//!   is in the live `Client` store. When the client drops, a `STALE_AFTER`
//!   grace covers reconnect blips, then the session is DEL'd. Tracking is
//!   in-memory (`disconnected_since`); daemon restart resets it, giving
//!   every reloaded session a fresh grace window.
//!
//! - **Pull/hook sessions** (`client_id: None`): an HTTP-MCP agent that
//!   registered via `/hook/session-register` or `/hook/session-start` has no
//!   WS client at all, so short-horizon elapsed time cannot distinguish a
//!   live, idle Codex TUI from a crashed one. The explicit
//!   `/hook/session-end` lifecycle hook owns clean removal, and the
//!   `PULL_STALE_AFTER` activity backstop reaps rows whose every hook signal
//!   has been silent for a day — without it, uncleanly-dead agents accrete
//!   durable sessions without bound (lv-6731).

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use hyphae::{Gettable, Materialize};
use marshal_entities::{
    AutoSource, Message, Room, RoomKind, RoomMember, Session, sanitize_room_description,
    sanitize_room_name,
};
use myko::{core::item::Eventable, server::MykoServerContext, utils::downcast_item};

/// How long a WS-shim session must be without a live client before DEL. Sized
/// to survive a WHOLE-FLEET reconnect after a daemon restart: on restart every
/// session replays from disk carrying a stale `client_id`, and the in-memory
/// `disconnected_since` map is empty, so all sessions enter the grace at once.
/// Too short and a slow-to-redial shim gets reaped — cascading its UNREAD
/// direct messages away (belongs_to(Session), review R5) — before it
/// reconnects to read them. 60s gives the fleet room to re-dial; a genuinely
/// dead session just lingers that long (and the roster's liveness fields show
/// it going stale).
pub const STALE_AFTER: Duration = Duration::from_secs(60);

/// Activity backstop for pull/hook sessions (`client_id: None`). They have no
/// WS client whose loss marks them dead, and `SessionEnd` only fires on a
/// clean exit — a crashed or discarded Codex/hook agent leaves its row
/// forever, which is how production accumulated ~2M durable sessions
/// (lv-6731). A pull session with no hook signal for this long is reaped.
///
/// Safe by construction: every prompt-submit hook idempotently re-registers
/// the session (see `hooks::register_hook_session`), so a live TUI idle past
/// the backstop gets its row repaired on its next prompt. The cost of a
/// false-positive reap is bounded — unread direct messages older than the
/// backstop cascade away with the row — while the cost of no backstop is
/// unbounded store growth. Sized generously above any plausible think-time
/// gap of a live session.
pub const PULL_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Messages older than this are pruned by `sweep_messages`, regardless of
/// recipient. Direct messages already cascade away with a DEL'd recipient
/// session; this bounds the ones that DON'T — broadcasts addressed to the
/// never-DEL'd `everyone`/`op:`/`project:` rooms, which otherwise accumulate
/// forever (review R6/D). DELing a Message cascades its `MessageRead` rows.
pub const MESSAGE_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Run the message-retention sweep every N session-sweeper ticks. The scan is
/// O(all messages); at one tick per `TICK_INTERVAL`, every 100 ticks (~5 min)
/// bounds growth without paying the scan each tick.
const MESSAGE_SWEEP_EVERY: u64 = 100;

/// How often the sweeper wakes up to check for stale sessions. Anything
/// roughly under `STALE_AFTER` is fine; the trade-off is reaction latency
/// (lower) vs. wake-ups per minute (higher).
pub const TICK_INTERVAL: Duration = Duration::from_secs(3);

/// Run the sweeper forever. Spawn this on a tokio task and forget it.
pub async fn run_sweeper(ctx: MykoServerContext) {
    let mut disconnected_since: HashMap<Arc<str>, Instant> = HashMap::new();
    let mut interval = tokio::time::interval(TICK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut tick: u64 = 0;

    loop {
        interval.tick().await;
        sweep_once(&ctx, &mut disconnected_since);
        sweep_rooms(&ctx);
        // Prune old messages on the first tick (boot cleanup) and every
        // MESSAGE_SWEEP_EVERY ticks thereafter — not every tick (the scan is
        // O(all messages)).
        if tick.is_multiple_of(MESSAGE_SWEEP_EVERY) {
            sweep_messages(&ctx);
        }
        tick = tick.wrapping_add(1);
    }
}

/// Prune messages older than `MESSAGE_TTL`. Bounds the never-cascaded
/// broadcasts (to `everyone`/`op:`/`project:`, which are never DEL'd) that
/// would otherwise grow the store without limit. DELing a Message cascades its
/// `MessageRead` rows via `belongs_to(Message)`, so read-state is cleaned too.
fn sweep_messages(ctx: &MykoServerContext) {
    let Some(store) = ctx.registry.get(Message::ENTITY_NAME_STATIC) else {
        return;
    };
    let cutoff = Utc::now().timestamp_millis() - MESSAGE_TTL.as_millis() as i64;
    let mut to_delete: Vec<Arc<str>> = Vec::new();
    for (id, item) in store.entries().materialize().get() {
        if let Some(m) = downcast_item::<Message>(&item)
            && m.sent_at < cutoff
        {
            to_delete.push(id);
        }
    }
    if to_delete.is_empty() {
        return;
    }
    log::info!(
        "[cleanup] pruning {} message(s) older than {} days",
        to_delete.len(),
        MESSAGE_TTL.as_secs() / 86_400,
    );
    for id in to_delete {
        if let Err(e) = ctx.del_by_id(Message::ENTITY_NAME_STATIC, &id) {
            log::warn!("[cleanup] prune message {} failed: {}", id, e);
        }
    }
}

/// GC pass: DEL auto-rooms that no longer earn their place — any `host:`
/// room (that anchor is retired; see `auto_rooms.rs`) and any auto-room that
/// has dropped to zero members (its anchoring sessions were all reaped, e.g.
/// the ~35 stale empties that accreted because room GC never existed).
/// `everyone` is permanent; adhoc rooms are user-owned and left alone.
/// DELing a Room cascades its RoomMember rows via `belongs_to(Room)`, so
/// live memberships never orphan.
fn sweep_rooms(ctx: &MykoServerContext) {
    repair_room_text(ctx);
    let Some(room_store) = ctx.registry.get(Room::ENTITY_NAME_STATIC) else {
        return;
    };
    let Some(member_store) = ctx.registry.get(RoomMember::ENTITY_NAME_STATIC) else {
        return;
    };

    // Live member count per room id.
    let mut member_counts: HashMap<Arc<str>, usize> = HashMap::new();
    for (_id, item) in member_store.entries().materialize().get() {
        if let Some(m) = downcast_item::<RoomMember>(&item) {
            *member_counts.entry(m.room_id.0.clone()).or_default() += 1;
        }
    }

    let mut to_delete: Vec<Arc<str>> = Vec::new();
    for (id, item) in room_store.entries().materialize().get() {
        let Some(room) = downcast_item::<Room>(&item) else {
            continue;
        };
        // Only auto-rooms are GC'd; adhoc rooms are user-owned.
        let RoomKind::Auto { source } = &room.kind else {
            continue;
        };
        // The global room is permanent.
        if matches!(source, AutoSource::Everyone) {
            continue;
        }
        let is_host = matches!(source, AutoSource::Host { .. });
        let empty = member_counts.get(&room.id.0).copied().unwrap_or(0) == 0;
        if is_host || empty {
            to_delete.push(id);
        }
    }

    for id in to_delete {
        log::info!("[cleanup] DELing stale auto-room {}", id);
        if let Err(e) = ctx.del_by_id(Room::ENTITY_NAME_STATIC, &id) {
            log::warn!("[cleanup] del room {} failed: {}", id, e);
        }
    }
}

/// Repair pass: re-SET any `Room` whose `name` or `description` breaks the
/// room text rule (`marshal_entities::is_forbidden_room_text_char` and the
/// length caps), with the text run through `sanitize_room_name` /
/// `sanitize_room_description` (a name that cleans to nothing falls back to
/// the cleaned id, then `"room"`). The id is never changed: members and
/// messages point at it. A room that is already clean is not written.
///
/// Defence in depth only. `JoinRoom` and the auto-room saga enforce the rule
/// at creation, but any WebSocket client can still SET a `Room` row directly
/// (myko applies client events as-is) or call the generated `SetRoomName` /
/// `SetRoomDescription` setters. This pass shortens how long such a name
/// stays stored, to one tick; it does not close that window, and a push sent
/// before the next tick still carries the raw name. The real close is
/// escaping room text where it is rendered into model context.
fn repair_room_text(ctx: &MykoServerContext) {
    let Some(room_store) = ctx.registry.get(Room::ENTITY_NAME_STATIC) else {
        return;
    };
    for (id, item) in room_store.entries().materialize().get() {
        let Some(room) = downcast_item::<Room>(&item) else {
            continue;
        };
        let name = repaired_room_name(&room.name, room.id.0.as_ref());
        let description = room.description.as_deref().map(sanitize_room_description);
        if name == room.name && description == room.description {
            continue;
        }
        log::warn!("[cleanup] repairing room {id:?}: name or description broke the room text rule");
        let repaired = Room {
            name,
            description,
            ..room
        };
        if let Err(e) = ctx.set(&repaired) {
            log::warn!("[cleanup] repair room {id:?} failed: {e}");
        }
    }
}

/// `name` with blocked characters dropped and the length capped. When that
/// leaves nothing, the room still needs a label: fall back to the id (run
/// through the same rule, since an old or directly-SET id can carry blocked
/// characters too), then to `"room"`.
fn repaired_room_name(name: &str, id: &str) -> String {
    let cleaned = sanitize_room_name(name);
    if !cleaned.is_empty() || name.is_empty() {
        return cleaned;
    }
    let from_id = sanitize_room_name(id);
    if from_id.is_empty() {
        "room".to_string()
    } else {
        from_id
    }
}

fn sweep_once(ctx: &MykoServerContext, disconnected_since: &mut HashMap<Arc<str>, Instant>) {
    let Some(session_store) = ctx.registry.get(Session::ENTITY_NAME_STATIC) else {
        return;
    };
    let Some(client_store) = ctx.registry.get("Client") else {
        // No Client store yet (server still warming up). Treat all sessions
        // as "live" this tick — wait until we have the snapshot before
        // making delete decisions.
        return;
    };

    let live_client_ids: HashSet<Arc<str>> = client_store
        .entries()
        .materialize()
        .get()
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    let now = Instant::now();
    let mut to_delete: Vec<Arc<str>> = Vec::new();
    let mut still_disconnected: HashSet<Arc<str>> = HashSet::new();

    for (id, item) in session_store.entries().materialize().get() {
        let Some(session) = downcast_item::<Session>(&item) else {
            continue;
        };

        match session.client_id.as_ref() {
            // Pull/hook session: no WS client by design, so short-horizon
            // elapsed time cannot tell a crashed client from a live idle TUI —
            // `SessionEnd` owns the prompt lifecycle and retaining the row
            // preserves nickname routing and unread direct messages while the
            // Codex process remains open. But an uncleanly-dead agent never
            // sends SessionEnd, so `PULL_STALE_AFTER` backstops it: reap when
            // every hook signal has been silent that long. A live TUI that
            // trips it is repaired on its next prompt (idempotent hook
            // re-registration).
            None => {
                let last_signal_ms = [
                    session.last_activity_at,
                    session.last_tool_at,
                    Some(session.connected_at),
                ]
                .into_iter()
                .flatten()
                .max()
                .unwrap_or(0);
                let now_ms = Utc::now().timestamp_millis();
                if now_ms.saturating_sub(last_signal_ms) >= PULL_STALE_AFTER.as_millis() as i64 {
                    to_delete.push(id);
                }
            }
            // WS shim session bound to a live client: healthy.
            Some(cid) if live_client_ids.contains(&cid.0) => {
                disconnected_since.remove(&id);
            }
            // WS shim session whose client has gone: reconnect grace.
            Some(_) => {
                still_disconnected.insert(id.clone());
                let first_seen = *disconnected_since.entry(id.clone()).or_insert(now);
                if now.duration_since(first_seen) >= STALE_AFTER {
                    to_delete.push(id);
                }
            }
        }
    }

    // Drop tracking for sessions that no longer exist in the store (e.g.
    // someone DEL'd them out from under us).
    disconnected_since.retain(|id, _| still_disconnected.contains(id));

    for id in to_delete {
        log::info!("[cleanup] DELing abandoned session {}", id);
        if let Err(e) = ctx.del_by_id(Session::ENTITY_NAME_STATIC, &id) {
            log::warn!("[cleanup] del session {} failed: {}", id, e);
            continue;
        }
        disconnected_since.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marshal_entities::{Message, MessageId, RoomId, RoomMemberId, SessionId};
    use myko::{
        server::Persister,
        wire::{MEvent, MEventType},
    };
    use myko_server::{BlackholePersister, MykoServer};
    use std::collections::HashSet;
    use uuid::Uuid;

    fn setup() -> MykoServerContext {
        marshal_entities::link();
        crate::link();
        let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
        let server = MykoServer::builder()
            .with_default_persister(blackhole)
            .build();
        let ctx = server.ctx();
        Box::leak(Box::new(server));
        ctx
    }

    fn set_hook_session(ctx: &MykoServerContext, id: &str, last_activity_at: i64) {
        let session = Session {
            id: SessionId(Arc::from(id)),
            client_id: None,
            pid: 0,
            cwd: "/repo".into(),
            git_branch: None,
            current_task: None,
            session_name: None,
            activity: None,
            kind: None,
            connected_at: 0,
            last_activity_at: Some(last_activity_at),
            last_tool: None,
            last_tool_at: None,
            operator: None,
            host: None,
            project: None,
            channels_enabled: None,
        };
        let event = MEvent::from_item(&session, MEventType::SET, &Uuid::new_v4().to_string());
        ctx.apply_event_batch(vec![event])
            .expect("apply hook Session SET");
    }

    fn session_ids(ctx: &MykoServerContext) -> HashSet<String> {
        ctx.registry
            .get(Session::ENTITY_NAME_STATIC)
            .map(|store| {
                store
                    .entries()
                    .materialize()
                    .get()
                    .into_iter()
                    .map(|(id, _)| id.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn sweep_preserves_idle_hook_sessions_within_the_backstop() {
        let ctx = setup();
        set_hook_session(&ctx, "idle-codex", Utc::now().timestamp_millis());

        sweep_once(&ctx, &mut HashMap::new());

        assert!(
            session_ids(&ctx).contains("idle-codex"),
            "short-horizon elapsed time cannot distinguish a live idle Codex TUI from a crash"
        );
    }

    /// Seed a Client row so the sweeper's warm-up gate (no Client store yet ⇒
    /// no delete decisions) sees a live snapshot, as in production.
    fn set_client(ctx: &MykoServerContext, id: &str) {
        let client = myko::entities::client::Client {
            id: myko::entities::client::ClientId(Arc::from(id)),
            server_id: myko::entities::server::ServerId(Arc::from("srv")),
            address: None,
            windback: None,
        };
        let ev = MEvent::from_item(&client, MEventType::SET, &Uuid::new_v4().to_string());
        ctx.apply_event_batch(vec![ev]).expect("apply Client SET");
    }

    #[test]
    fn sweep_reaps_hook_sessions_past_the_activity_backstop() {
        let ctx = setup();
        set_client(&ctx, "ws-client");
        let stale = Utc::now().timestamp_millis() - PULL_STALE_AFTER.as_millis() as i64 - 60_000;
        set_hook_session(&ctx, "dead-codex", stale);
        set_hook_session(&ctx, "live-codex", Utc::now().timestamp_millis());

        sweep_once(&ctx, &mut HashMap::new());

        let ids = session_ids(&ctx);
        assert!(
            !ids.contains("dead-codex"),
            "a pull session silent past PULL_STALE_AFTER is unboundedly-retained garbage (lv-6731)"
        );
        assert!(ids.contains("live-codex"));
    }

    fn set_room(ctx: &MykoServerContext, id: &str, kind: RoomKind) {
        let room = Room {
            id: RoomId(Arc::from(id)),
            name: id.to_string(),
            description: None,
            kind,
            created_at: 0,
        };
        let ev = MEvent::from_item(&room, MEventType::SET, &Uuid::new_v4().to_string());
        ctx.apply_event_batch(vec![ev]).expect("apply Room SET");
    }

    fn set_member(ctx: &MykoServerContext, room_id: &str, session_id: &str) {
        let member = RoomMember {
            id: RoomMemberId(Arc::from(RoomMember::make_id(room_id, session_id).as_str())),
            room_id: RoomId(Arc::from(room_id)),
            session_id: SessionId(Arc::from(session_id)),
            joined_at: 0,
        };
        let ev = MEvent::from_item(&member, MEventType::SET, &Uuid::new_v4().to_string());
        ctx.apply_event_batch(vec![ev])
            .expect("apply RoomMember SET");
    }

    fn room_ids(ctx: &MykoServerContext) -> HashSet<String> {
        ctx.registry
            .get(Room::ENTITY_NAME_STATIC)
            .map(|s| {
                s.entries()
                    .materialize()
                    .get()
                    .into_iter()
                    .map(|(id, _)| id.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn sweep_reaps_host_and_empty_auto_rooms_and_spares_the_rest() {
        let ctx = setup();

        // Survives: the global room, always.
        set_room(
            &ctx,
            "everyone",
            RoomKind::Auto {
                source: AutoSource::Everyone,
            },
        );
        set_member(&ctx, "everyone", "sess-a");
        // Reaped: a host room, even with a live member (anchor retired).
        set_room(
            &ctx,
            "host:node1",
            RoomKind::Auto {
                source: AutoSource::Host {
                    name: "node1".into(),
                },
            },
        );
        set_member(&ctx, "host:node1", "sess-a");
        // Survives: a project room with members.
        set_room(
            &ctx,
            "project:live",
            RoomKind::Auto {
                source: AutoSource::Project {
                    basename: "live".into(),
                },
            },
        );
        set_member(&ctx, "project:live", "sess-a");
        // Reaped: a project room whose sessions were all reaped (0 members).
        set_room(
            &ctx,
            "project:stale",
            RoomKind::Auto {
                source: AutoSource::Project {
                    basename: "stale".into(),
                },
            },
        );
        // Survives: an adhoc room even when empty — user-owned lifecycle.
        set_room(&ctx, "design-sync", RoomKind::Adhoc);

        sweep_rooms(&ctx);

        let ids = room_ids(&ctx);
        assert!(ids.contains("everyone"), "global room must survive");
        assert!(
            ids.contains("project:live"),
            "populated project room must survive"
        );
        assert!(ids.contains("design-sync"), "empty adhoc room must survive");
        assert!(!ids.contains("host:node1"), "host room must be reaped");
        assert!(
            !ids.contains("project:stale"),
            "empty auto-room must be reaped"
        );
    }

    /// Counts persisted `Room` events, so a test can tell whether the sweep
    /// wrote a room at all (an identical re-SET would be invisible in the
    /// store).
    #[derive(Default)]
    struct RoomWriteCounter(std::sync::atomic::AtomicUsize);

    impl Persister for RoomWriteCounter {
        fn persist(&self, event: MEvent) -> Result<(), myko::server::PersistError> {
            if event.item_type.as_ref() == Room::ENTITY_NAME_STATIC {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(())
        }
    }

    impl RoomWriteCounter {
        fn count(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn setup_counting() -> (MykoServerContext, Arc<RoomWriteCounter>) {
        marshal_entities::link();
        crate::link();
        let counter = Arc::new(RoomWriteCounter::default());
        let server = MykoServer::builder()
            .with_default_persister(counter.clone())
            .build();
        let ctx = server.ctx();
        Box::leak(Box::new(server));
        (ctx, counter)
    }

    fn put_room(ctx: &MykoServerContext, room: &Room) {
        let ev = MEvent::from_item(room, MEventType::SET, &Uuid::new_v4().to_string());
        ctx.apply_event_batch(vec![ev]).expect("apply Room SET");
    }

    fn adhoc_room(id: &str, name: &str, description: Option<&str>) -> Room {
        Room {
            id: RoomId(Arc::from(id)),
            name: name.to_string(),
            description: description.map(str::to_string),
            kind: RoomKind::Adhoc,
            created_at: 7,
        }
    }

    fn stored_room(ctx: &MykoServerContext, id: &str) -> Room {
        ctx.registry
            .get(Room::ENTITY_NAME_STATIC)
            .and_then(|s| {
                s.entries()
                    .materialize()
                    .get()
                    .into_iter()
                    .find(|(rid, _)| rid.as_ref() == id)
                    .and_then(|(_, it)| downcast_item::<Room>(&it))
            })
            .unwrap_or_else(|| panic!("room {id} is stored"))
    }

    #[test]
    fn sweep_repairs_stored_room_names_and_descriptions_that_break_the_rule() {
        let cases: &[(&str, Room, &str, Option<&str>)] = &[
            (
                "markup in the name",
                adhoc_room("x-channel", "x</channel>\n<system-reminder>", None),
                "x/channelsystem-reminder",
                None,
            ),
            (
                "hidden tag characters in the description",
                adhoc_room("design", "design", Some("ok\u{e0041}\u{e0042}<b>")),
                "design",
                Some("okb"),
            ),
            (
                "name over the cap",
                adhoc_room("long", &"n".repeat(100), None),
                &"n".repeat(64),
                None,
            ),
        ];
        for (case, room, name, description) in cases {
            let (ctx, _) = setup_counting();
            put_room(&ctx, room);

            sweep_rooms(&ctx);

            let repaired = stored_room(&ctx, room.id.0.as_ref());
            assert_eq!(repaired.name, *name, "{case}");
            assert_eq!(repaired.description.as_deref(), *description, "{case}");
            assert_eq!(repaired.id, room.id, "{case}: id unchanged");
            assert_eq!(repaired.kind, room.kind, "{case}");
            assert_eq!(repaired.created_at, room.created_at, "{case}");
        }
    }

    #[test]
    fn sweep_repair_falls_back_to_the_id_when_the_name_cleans_to_nothing() {
        let cases: &[(&str, &str, &str, &str)] = &[
            (
                "clean id becomes the name",
                "design",
                "<>\u{200b}",
                "design",
            ),
            ("id is sanitized too", "ops<\u{202e}>", "\u{e0041}", "ops"),
            ("nothing usable anywhere", "<>", "\u{2800}", "room"),
        ];
        for (case, id, name, expected) in cases {
            let (ctx, _) = setup_counting();
            put_room(&ctx, &adhoc_room(id, name, None));

            sweep_rooms(&ctx);

            let repaired = stored_room(&ctx, id);
            assert_eq!(repaired.name, *expected, "{case}");
            assert_eq!(repaired.id.0.as_ref(), *id, "{case}: id unchanged");
        }
    }

    #[test]
    fn sweep_repair_keeps_the_id_so_members_stay_attached() {
        let (ctx, _) = setup_counting();
        put_room(&ctx, &adhoc_room("evil", "evil\u{202e}", None));
        set_member(&ctx, "evil", "sess-a");

        sweep_rooms(&ctx);

        assert_eq!(stored_room(&ctx, "evil").name, "evil");
        let members = ctx
            .registry
            .get(RoomMember::ENTITY_NAME_STATIC)
            .map(|s| s.entries().materialize().get().len())
            .unwrap_or_default();
        assert_eq!(members, 1, "the membership row survives the repair");
    }

    #[test]
    fn sweep_does_not_write_rooms_that_are_already_clean() {
        let (ctx, writes) = setup_counting();
        put_room(
            &ctx,
            &adhoc_room("design", "Design sync", Some("Weekly ❤️ sync")),
        );
        put_room(
            &ctx,
            &Room {
                id: RoomId(Arc::from("everyone")),
                name: "everyone".into(),
                description: None,
                kind: RoomKind::Auto {
                    source: AutoSource::Everyone,
                },
                created_at: 0,
            },
        );
        let before = writes.count();

        sweep_rooms(&ctx);

        assert_eq!(writes.count(), before, "a clean room produces no write");

        // Positive control: the counter does see a repair.
        put_room(&ctx, &adhoc_room("bad", "bad<", None));
        let before = writes.count();
        sweep_rooms(&ctx);
        assert_eq!(writes.count(), before + 1, "a dirty room is written once");
    }

    fn set_message(ctx: &MykoServerContext, id: &str, sent_at: i64) {
        let msg = Message {
            id: MessageId(Arc::from(id)),
            from_session_id: SessionId(Arc::from("sender")),
            to_session_id: Some(SessionId(Arc::from("recipient"))),
            to_room_id: None,
            to_operator: None,
            body: "hi".into(),
            sent_at,
        };
        let ev = MEvent::from_item(&msg, MEventType::SET, &Uuid::new_v4().to_string());
        ctx.apply_event_batch(vec![ev]).expect("apply Message SET");
    }

    fn message_ids(ctx: &MykoServerContext) -> HashSet<String> {
        ctx.registry
            .get(Message::ENTITY_NAME_STATIC)
            .map(|s| {
                s.entries()
                    .materialize()
                    .get()
                    .into_iter()
                    .map(|(id, _)| id.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn sweep_messages_prunes_old_and_keeps_recent() {
        let ctx = setup();
        let now = chrono::Utc::now().timestamp_millis();
        // Older than the TTL → pruned.
        set_message(&ctx, "old", now - MESSAGE_TTL.as_millis() as i64 - 1);
        // Well within the TTL → kept.
        set_message(&ctx, "recent", now - 1_000);

        sweep_messages(&ctx);

        let ids = message_ids(&ctx);
        assert!(
            !ids.contains("old"),
            "message older than the TTL must be pruned"
        );
        assert!(ids.contains("recent"), "recent message must survive");
    }
}
