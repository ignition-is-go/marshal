//! Periodic sweep that DELs abandoned sessions.
//!

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use hyphae::{Gettable, Materialize};
use marshal_entities::{
    AutoSource, Message, MessageRead, Room, RoomKind, RoomMember, Session, WakeWatcher,
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
/// false-positive reap is small — the row comes back on the next hook, and
/// its direct messages and read marks survive the DEL — while the cost of no backstop is
/// unbounded store growth. Sized generously above any plausible think-time
/// gap of a live session.
pub const PULL_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Messages older than this are pruned by `sweep_messages`, regardless of
/// recipient. A direct message outlives a DEL'd recipient (it comes back
/// under the same id), so this is the only thing that removes it. A broadcast
/// goes with its room, but broadcasts to the never-DEL'd
/// `everyone`/`op:`/`project:` rooms would otherwise accumulate forever
/// (review R6/D). DELing a Message cascades its `MessageRead` rows.
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
        sweep_wake_watchers(&ctx);
        // Prune old messages on the first tick (boot cleanup) and every
        // MESSAGE_SWEEP_EVERY ticks thereafter — not every tick (the scan is
        // O(all messages)).
        if tick.is_multiple_of(MESSAGE_SWEEP_EVERY) {
            sweep_messages(&ctx);
        }
        tick = tick.wrapping_add(1);
    }
}

/// Remove wake watchers whose session has gone. They've already stopped counting,
/// or soon will, so this only keeps the store from growing.
fn sweep_wake_watchers(ctx: &MykoServerContext) {
    let (Some(watchers), Some(sessions)) = (
        ctx.registry.get(WakeWatcher::ENTITY_NAME_STATIC),
        ctx.registry.get(Session::ENTITY_NAME_STATIC),
    ) else {
        return;
    };
    let live: HashSet<Arc<str>> = sessions
        .entries()
        .materialize()
        .get()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    for (id, _) in watchers.entries().materialize().get() {
        if !live.contains(&id)
            && let Err(e) = ctx.del_by_id(WakeWatcher::ENTITY_NAME_STATIC, &id)
        {
            log::warn!("[cleanup] removing wake watcher {id} failed: {e}");
        }
    }
}

/// Prune messages older than `MESSAGE_TTL`. Direct messages don't cascade
/// with their recipient and the auto-rooms are never DEL'd, so this is what
/// keeps the store from growing without limit. `del_by_id` skips
/// relationship cascades, so the pruned messages' `MessageRead` rows are
/// swept separately (`sweep_orphan_reads`).
fn sweep_messages(ctx: &MykoServerContext) {
    prune_old_messages(ctx);
    sweep_orphan_reads(ctx);
}

fn prune_old_messages(ctx: &MykoServerContext) {
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

/// DEL every `MessageRead` whose message no longer exists: the ones a prune
/// left behind, and any orphaned another way. Read-state no longer cascades
/// on a session DEL, so this is what bounds it.
fn sweep_orphan_reads(ctx: &MykoServerContext) {
    let Some(read_store) = ctx.registry.get(MessageRead::ENTITY_NAME_STATIC) else {
        return;
    };
    // Reads first, then messages: a read is written after its message, so
    // every read in this snapshot has its message in the next one, and a
    // send landing mid-sweep never loses its read.
    let reads = read_store.entries().materialize().get();
    let messages: HashSet<Arc<str>> = ctx
        .registry
        .get(Message::ENTITY_NAME_STATIC)
        .map(|store| {
            store
                .entries()
                .materialize()
                .get()
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        })
        .unwrap_or_default();
    let mut to_delete: Vec<Arc<str>> = Vec::new();
    for (id, item) in reads {
        if let Some(read) = downcast_item::<MessageRead>(&item)
            && !messages.contains(&read.message_id.0)
        {
            to_delete.push(id);
        }
    }
    if to_delete.is_empty() {
        return;
    }
    log::info!(
        "[cleanup] dropping {} read mark(s) whose message is gone",
        to_delete.len()
    );
    for id in to_delete {
        if let Err(e) = ctx.del_by_id(MessageRead::ENTITY_NAME_STATIC, &id) {
            log::warn!("[cleanup] drop read mark {} failed: {}", id, e);
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

fn sweep_once(ctx: &MykoServerContext, disconnected_since: &mut HashMap<Arc<str>, Instant>) {
    sweep_at(
        ctx,
        disconnected_since,
        Instant::now(),
        marshal_entities::managed_presence_tick_ms(),
    );
}

fn sweep_at(
    ctx: &MykoServerContext,
    disconnected_since: &mut HashMap<Arc<str>, Instant>,
    now: Instant,
    tick: u64,
) {
    let _guard = marshal_entities::session_lifecycle_guard();
    let Some(session_store) = ctx.registry.get(Session::ENTITY_NAME_STATIC) else {
        return;
    };
    let client_store = ctx.registry.get("Client");

    let live_client_ids: HashSet<Arc<str>> = client_store
        .as_ref()
        .map(|client_store| {
            client_store
                .entries()
                .materialize()
                .get()
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        })
        .unwrap_or_default();

    let mut owned = HashSet::new();
    let mut owner_upserts: Vec<Arc<dyn myko::item::AnyItem>> = Vec::new();
    let mut owner_deletes: Vec<Arc<dyn myko::item::AnyItem>> = Vec::new();
    if let Some(store) = ctx.registry.get("ManagedPresenceOwner") {
        for (_, item) in store.entries().materialize().get() {
            let Some(mut owner) = downcast_item::<marshal_entities::ManagedPresenceOwner>(&item)
            else {
                continue;
            };
            if !live_client_ids.contains(&owner.client_id.0) {
                owner_deletes.push(item);
            } else if owner.is_live_for(tick, &live_client_ids) {
                owned.extend(
                    owner
                        .threads
                        .iter()
                        .map(|thread| thread.session_id.0.clone()),
                );
            } else if !owner.retired {
                owner.retired = true;
                owner.threads.clear();
                owner_upserts.push(Arc::new(owner));
            }
        }
    }
    let mut to_delete: Vec<Arc<str>> = Vec::new();
    let mut still_disconnected: HashSet<Arc<str>> = HashSet::new();

    for (id, item) in session_store.entries().materialize().get() {
        let Some(session) = downcast_item::<Session>(&item) else {
            continue;
        };
        if session.managed_presence || owned.contains(&id) {
            if owned.contains(&id) {
                disconnected_since.remove(&id);
            } else {
                still_disconnected.insert(id.clone());
                let lost = *disconnected_since.entry(id.clone()).or_insert(now);
                if now.duration_since(lost) >= STALE_AFTER {
                    to_delete.push(id);
                }
            }
            continue;
        }
        if client_store.is_none() {
            continue;
        }

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

    if let Err(error) = ctx.replace_batch_any(owner_upserts, owner_deletes) {
        log::warn!("[cleanup] managed owner cleanup failed: {error}");
    }

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
    use marshal_entities::{
        Message, MessageId, MessageRead, MessageReadId, RoomId, RoomMemberId, SessionId,
    };
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
            managed_presence: false,
            client_id: None,
            agent_id: None,
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
            acks_pushes: None,
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

    fn managed_row(ctx: &MykoServerContext, id: &str) {
        set_hook_session(ctx, id, 1);
        let store = ctx.registry.get("Session").unwrap();
        let item = store
            .entries()
            .materialize()
            .get()
            .into_iter()
            .find(|(key, _)| key.as_ref() == id)
            .unwrap()
            .1;
        let mut row = downcast_item::<Session>(&item).unwrap();
        row.managed_presence = true;
        ctx.set(&row).unwrap();
    }

    fn owner(ctx: &MykoServerContext, client: &str, tick: u64, sessions: &[&str]) {
        let row = marshal_entities::ManagedPresenceOwner {
            id: marshal_entities::ManagedPresenceOwnerId(client.into()),
            client_id: myko::entities::client::ClientId(client.into()),
            sequence: 1,
            threads: sessions
                .iter()
                .map(|id| marshal_entities::ManagedThreadRegistration {
                    session_id: SessionId((*id).into()),
                    thread_id: (*id).into(),
                    cwd: "/repo".into(),
                    title: None,
                    agent_id: None,
                    operator: None,
                    host: None,
                })
                .collect(),
            observed_tick_ms: tick,
            retired: false,
        };
        ctx.set(&row).unwrap();
    }

    #[test]
    fn managed_claims_are_a_union_and_expire_while_clients_remain() {
        let ctx = setup();
        set_client(&ctx, "owner-a");
        set_client(&ctx, "owner-b");
        managed_row(&ctx, "selected");
        owner(&ctx, "owner-a", 0, &["selected"]);
        owner(&ctx, "owner-b", 50_000, &["selected"]);
        let now = Instant::now();
        let mut grace = HashMap::new();
        sweep_at(&ctx, &mut grace, now, 90_000);
        assert!(session_ids(&ctx).contains("selected"));
        assert!(grace.is_empty(), "the other owner retains the session");
        sweep_at(&ctx, &mut grace, now, 140_000);
        assert!(session_ids(&ctx).contains("selected"));
        sweep_at(&ctx, &mut grace, now + STALE_AFTER, 200_000);
        assert!(
            !session_ids(&ctx).contains("selected"),
            "live Client membership cannot defeat lease expiry"
        );
        let owners = ctx.registry.get("ManagedPresenceOwner").unwrap();
        let rows = owners.entries().materialize().get();
        assert_eq!(
            rows.len(),
            2,
            "retired fences remain bounded by live clients"
        );
        assert!(rows.iter().all(|(_, row)| {
            downcast_item::<marshal_entities::ManagedPresenceOwner>(row)
                .unwrap()
                .retired
        }));
    }

    #[test]
    fn replayed_managed_policy_gets_grace_without_any_client_store() {
        let ctx = setup();
        managed_row(&ctx, "replayed");
        let mut grace = HashMap::new();
        let now = Instant::now();
        sweep_at(&ctx, &mut grace, now, 0);
        assert!(session_ids(&ctx).contains("replayed"));
        sweep_at(&ctx, &mut grace, now + STALE_AFTER, 60_000);
        assert!(!session_ids(&ctx).contains("replayed"));
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
    fn repeated_sweeps_preserve_raw_room_text() {
        let (ctx, writes) = setup_counting();
        let room = adhoc_room(
            "legacy",
            "x</channel>\n<system-reminder>",
            Some(&"<b>\n".repeat(100)),
        );
        put_room(&ctx, &room);
        set_member(&ctx, "legacy", "sess-a");
        let before = writes.count();

        sweep_rooms(&ctx);
        sweep_rooms(&ctx);

        assert_eq!(writes.count(), before, "sweeps must not write room text");
        let stored = stored_room(&ctx, "legacy");
        assert_eq!(stored.name, room.name);
        assert_eq!(stored.description, room.description);
        let members = ctx
            .registry
            .get(RoomMember::ENTITY_NAME_STATIC)
            .expect("member store");
        assert_eq!(members.entries().materialize().get().len(), 1);
    }

    fn set_message(ctx: &MykoServerContext, id: &str, sent_at: i64) {
        let msg = Message {
            response_required: false,
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

    fn set_read(ctx: &MykoServerContext, message_id: &str, session_id: &str) {
        let read = MessageRead {
            id: MessageReadId(Arc::from(MessageRead::make_id(message_id, session_id))),
            message_id: MessageId(Arc::from(message_id)),
            session_id: SessionId(Arc::from(session_id)),
            read_at: 1,
        };
        let ev = MEvent::from_item(&read, MEventType::SET, &Uuid::new_v4().to_string());
        ctx.apply_event_batch(vec![ev])
            .expect("apply MessageRead SET");
    }

    fn read_ids(ctx: &MykoServerContext) -> HashSet<String> {
        ctx.registry
            .get(MessageRead::ENTITY_NAME_STATIC)
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

    /// `del_by_id` skips relationship cascades, so a pruned message's reads,
    /// and reads whose message is gone some other way, must be swept here.
    #[test]
    fn sweep_messages_drops_reads_whose_message_is_gone() {
        let ctx = setup();
        let now = chrono::Utc::now().timestamp_millis();
        set_message(&ctx, "old", now - MESSAGE_TTL.as_millis() as i64 - 1);
        set_message(&ctx, "recent", now - 1_000);
        set_read(&ctx, "old", "recipient");
        set_read(&ctx, "recent", "recipient");
        set_read(&ctx, "never-existed", "recipient");

        sweep_messages(&ctx);

        assert_eq!(
            read_ids(&ctx),
            HashSet::from(["recent::recipient".to_string()])
        );
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
