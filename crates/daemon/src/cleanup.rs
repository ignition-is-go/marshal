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
//!   registered via the `/hook/session-start` endpoint has no WS client at
//!   all — it would be swept instantly by the client-id rule. Its liveness
//!   is instead its `last_activity_at` (the hook bumps it every turn) plus
//!   a long `HOOK_BACKSTOP` grace. The clean teardown path is the explicit
//!   `/hook/session-end` DEL; this grace is only a backstop for a client
//!   that crashed without firing SessionEnd. Because `last_activity_at` is
//!   wall-clock and persisted, the backstop survives daemon restarts.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use hyphae::Gettable;
use marshal_entities::Session;
use myko::{core::item::Eventable, server::CellServerCtx, utils::downcast_item};

/// How long a WS-shim session must be without a live client before DEL.
pub const STALE_AFTER: Duration = Duration::from_secs(10);

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

    loop {
        interval.tick().await;
        sweep_once(&ctx, &mut disconnected_since);
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
