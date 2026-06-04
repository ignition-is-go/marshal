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

use std::{
    collections::HashSet,
    sync::Arc,
    time::Duration,
};

use marshal_entities::Message;
use myko::{
    core::item::Eventable,
    server::CellServerCtx,
    utils::downcast_item,
};
use serde_json::json;

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

    for (id, item) in store.snapshot() {
        if seen.contains(&id) {
            continue;
        }
        seen.insert(id.clone());

        let Some(msg) = downcast_item::<Message>(&item) else {
            continue;
        };

        // Direct send only for now. Broadcasts require resolving the
        // room's members at push time; deferred.
        let Some(to_sid) = &msg.to_session_id else {
            continue;
        };

        let Some(channel) = sse_channels.get(to_sid.0.as_ref()) else {
            // Recipient is shim-connected (no SSE channel registered on
            // this daemon) — they get the message via the existing
            // shim NotifyChannel path. Skip.
            continue;
        };

        // Frame the message as a Claude Code channel notification. The
        // exact shape mirrors what the shim emits today, so an agent
        // can't tell the difference between shim-delivered and
        // SSE-delivered peer messages.
        let params = json!({
            "channel": "marshal",
            "data": {
                "from_session_id": msg.from_session_id.0.as_ref(),
                "from_nick": msg.from_nick,
                "body": msg.body,
                "sent_at": msg.sent_at,
            },
        });
        if !channel.send_notification("notifications/claude/channel", params) {
            // SSE channel is gone (recipient disconnected) — drop the
            // entry so we don't keep retrying.
            sse_channels.get(to_sid.0.as_ref()); // no-op; observer's
            // `Ended` event will eventually remove it.
            log::debug!(
                "[push] SSE channel for {} closed before push could send",
                to_sid.0.as_ref()
            );
        }
    }
}
