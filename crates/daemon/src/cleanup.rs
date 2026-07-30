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
//!   WS client at all — it would be swept instantly by the client-id rule. Its
//!   liveness is instead its `last_activity_at` (the hooks bump it every turn)
//!   plus a long `HOOK_BACKSTOP` grace. The clean teardown path is the explicit
//!   `/hook/session-end` DEL; this grace is only a backstop for a client that
//!   crashed without firing SessionEnd. Because `last_activity_at` is
//!   wall-clock and persisted, the backstop survives daemon restarts.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use hyphae::Gettable;
use marshal_entities::{AutoSource, Message, Room, RoomKind, RoomMember, Session};
use myko::{core::item::Eventable, server::CellServerCtx, utils::downcast_item};

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

/// How long a pull/hook session (no WS client) may go without any hook
/// activity before the backstop DELs it. Generous: merely-idle sessions
/// re-register on their next turn, so this only needs to be short enough
/// to eventually reclaim sessions whose client crashed without firing
/// `/hook/session-end`. 60 min.
pub const HOOK_BACKSTOP: Duration = Duration::from_secs(60 * 60);

/// How often the sweeper wakes up to check for stale sessions. Anything
/// roughly under `STALE_AFTER` is fine; the trade-off is reaction latency
/// (lower) vs. wake-ups per minute (higher).
pub const TICK_INTERVAL: Duration = Duration::from_secs(3);

/// Run the sweeper forever. Spawn this on a tokio task and forget it.
pub async fn run_sweeper(ctx: CellServerCtx) {
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
fn sweep_messages(ctx: &CellServerCtx) {
    let Some(store) = ctx.registry.get(Message::ENTITY_NAME_STATIC) else {
        return;
    };
    let cutoff = Utc::now().timestamp_millis() - MESSAGE_TTL.as_millis() as i64;
    let mut to_delete: Vec<Arc<str>> = Vec::new();
    for (id, item) in store.entries().get() {
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
fn sweep_rooms(ctx: &CellServerCtx) {
    let Some(room_store) = ctx.registry.get(Room::ENTITY_NAME_STATIC) else {
        return;
    };
    let Some(member_store) = ctx.registry.get(RoomMember::ENTITY_NAME_STATIC) else {
        return;
    };

    // Live member count per room id.
    let mut member_counts: HashMap<Arc<str>, usize> = HashMap::new();
    for (_id, item) in member_store.entries().get() {
        if let Some(m) = downcast_item::<RoomMember>(&item) {
            *member_counts.entry(m.room_id.0.clone()).or_default() += 1;
        }
    }

    let mut to_delete: Vec<Arc<str>> = Vec::new();
    for (id, item) in room_store.entries().get() {
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

fn sweep_once(ctx: &CellServerCtx, disconnected_since: &mut HashMap<Arc<str>, Instant>) {
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
        .get()
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    let now = Instant::now();
    let now_ms = Utc::now().timestamp_millis();
    let backstop_ms = HOOK_BACKSTOP.as_millis() as i64;
    let mut to_delete: Vec<Arc<str>> = Vec::new();
    let mut still_disconnected: HashSet<Arc<str>> = HashSet::new();

    for (id, item) in session_store.entries().get() {
        let Some(session) = downcast_item::<Session>(&item) else {
            continue;
        };

        match session.client_id.as_ref() {
            // Pull/hook session: no WS client by design. Liveness is hook
            // activity + the long backstop; the SessionEnd hook is the
            // clean DEL path. Not subject to the WS reconnect grace.
            None => {
                let last = session.last_activity_at.unwrap_or(session.connected_at);
                if now_ms.saturating_sub(last) >= backstop_ms {
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
    use myko_server::{BlackholePersister, CellServer};
    use std::collections::HashSet;
    use uuid::Uuid;

    fn setup() -> CellServerCtx {
        marshal_entities::link();
        crate::link();
        let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
        let server = CellServer::builder()
            .with_default_persister(blackhole)
            .build();
        let ctx = server.ctx();
        Box::leak(Box::new(server));
        ctx
    }

    fn set_room(ctx: &CellServerCtx, id: &str, kind: RoomKind) {
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

    fn set_member(ctx: &CellServerCtx, room_id: &str, session_id: &str) {
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

    fn room_ids(ctx: &CellServerCtx) -> HashSet<String> {
        ctx.registry
            .get(Room::ENTITY_NAME_STATIC)
            .map(|s| {
                s.entries()
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

    fn set_message(ctx: &CellServerCtx, id: &str, sent_at: i64) {
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

    fn message_ids(ctx: &CellServerCtx) -> HashSet<String> {
        ctx.registry
            .get(Message::ENTITY_NAME_STATIC)
            .map(|s| {
                s.entries()
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
