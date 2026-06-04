//! Forward new `Message` entities into the SSE streams of HTTP-MCP
//! recipients.
//!
//! The shim today wires this as a `client.on_command::<NotifyChannel>`
//! handler that forwards into stdio MCP. For HTTP-connected agents
//! there is no MykoClient and no stdio — they hold an SSE channel
//! instead. This task watches the `Message` registry, finds new direct
//! messages targeted at sessions that have an SSE channel registered
//! in `SseChannels`, and pushes a `notifications/claude/channel` frame
//! over that channel.
//!
//! Implementation note: polling. Myko's reactive-cell subscription
//! primitives are designed for the client side (`watch_query`); the
//! server-side analog is open work. A 200 ms poll has acceptable
//! latency for inline chat-style notifications and keeps the code small.
//! When server-side reactive subscriptions land in myko, this loop can
//! be replaced with an event-driven watcher.

use std::{collections::HashSet, sync::Arc, time::Duration};

use marshal_entities::{Message, RoomMember};
use myko::{core::item::Eventable, server::CellServerCtx, utils::downcast_item};
use serde_json::{Value, json};

use crate::mcp_observer::SseChannels;

/// How often to scan the Message store for new entries. Lower → snappier
/// inline notifications, more wakeups per second. 200 ms is well below
/// the perceptual threshold for "instant" without burning CPU.
pub const TICK_INTERVAL: Duration = Duration::from_millis(200);

/// Run the push loop forever. Spawn this on a tokio task and forget it.
pub async fn run_push_loop(ctx: CellServerCtx, sse_channels: SseChannels) {
    let mut seen: HashSet<Arc<str>> = HashSet::new();
    // Prime: anything already in the registry on startup is "history" —
    // don't replay it as a push. New peer messages get added later.
    if let Some(store) = ctx.registry.get(Message::ENTITY_NAME_STATIC) {
        for (id, _) in store.snapshot() {
            seen.insert(id);
        }
    }

    let mut interval = tokio::time::interval(TICK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        tick_once(&ctx, &sse_channels, &mut seen);
    }
}

fn tick_once(
    ctx: &CellServerCtx,
    sse_channels: &SseChannels,
    seen: &mut HashSet<Arc<str>>,
) {
    let Some(store) = ctx.registry.get(Message::ENTITY_NAME_STATIC) else {
        return;
    };

    // Snapshot room membership once per tick so a broadcast doesn't pay
    // a per-recipient registry lookup. Cheap — RoomMember rows are
    // O(sessions × rooms) which is small in practice.
    let members_by_room: std::collections::HashMap<Arc<str>, Vec<Arc<str>>> = ctx
        .registry
        .get(RoomMember::ENTITY_NAME_STATIC)
        .map(|store| {
            let mut acc: std::collections::HashMap<Arc<str>, Vec<Arc<str>>> = Default::default();
            for (_id, item) in store.snapshot() {
                if let Some(rm) = downcast_item::<RoomMember>(&item) {
                    acc.entry(rm.room_id.0.clone())
                        .or_default()
                        .push(rm.session_id.0.clone());
                }
            }
            acc
        })
        .unwrap_or_default();

    for (id, item) in store.snapshot() {
        if seen.contains(&id) {
            continue;
        }
        seen.insert(id.clone());

        let Some(msg) = downcast_item::<Message>(&item) else {
            continue;
        };

        // Build the params once — same shape regardless of routing
        // path. Mirrors the shim's `notifications/claude/channel`
        // emission so agents can't tell the difference between
        // shim-delivered and SSE-delivered peer messages.
        let params = channel_params(&msg);

        // Route by addressing:
        // - to_session_id set → direct send to that session's SSE if open
        // - to_room_id set → fan out to every member with an open SSE,
        //   skipping the sender themselves so a broadcast doesn't echo
        //   back into the sender's transcript
        if let Some(to_sid) = &msg.to_session_id {
            push_to_session(sse_channels, to_sid.0.as_ref(), &params);
        } else if let Some(to_room) = &msg.to_room_id {
            if let Some(members) = members_by_room.get(to_room.0.as_ref()) {
                for member_sid in members {
                    if member_sid.as_ref() == msg.from_session_id.0.as_ref() {
                        continue;
                    }
                    push_to_session(sse_channels, member_sid.as_ref(), &params);
                }
            }
        }
    }
}

fn channel_params(msg: &Message) -> Value {
    json!({
        "channel": "marshal",
        "data": {
            "from_session_id": msg.from_session_id.0.as_ref(),
            "from_nick": msg.from_nick,
            "body": msg.body,
            "sent_at": msg.sent_at,
            "to_session_id": msg.to_session_id.as_ref().map(|s| s.0.as_ref()),
            "to_room_id": msg.to_room_id.as_ref().map(|r| r.0.as_ref()),
        },
    })
}

fn push_to_session(sse_channels: &SseChannels, session_id: &str, params: &Value) {
    let Some(channel) = sse_channels.get(session_id) else {
        // Recipient is shim-connected (no SSE channel registered on
        // this daemon) or just not subscribed — they get the message
        // via shim NotifyChannel path or by querying
        // `query_GetAllMessages`. Skip.
        return;
    };
    if !channel.send_notification("notifications/claude/channel", params.clone()) {
        log::debug!(
            "[push] SSE channel for {} closed before push could send",
            session_id
        );
    }
}
