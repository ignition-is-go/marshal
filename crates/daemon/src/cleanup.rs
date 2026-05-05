//! Periodic sweep that DELs sessions whose bound client has been gone for
//! more than `STALE_AFTER`.
//!
//! Sessions intentionally outlive their clients (Session no longer has a
//! `belongs_to(Client)` cascade) so the roster can show a brief
//! "disconnected" indicator across reconnect blips. Without this sweeper
//! that grace period would last forever — abandoned sessions would
//! accumulate in the registry and the on-disk log.
//!
//! Tracking is in-memory: a `disconnected_since` map records the first
//! tick on which each session was observed disconnected. Daemon restart
//! resets it, which is fine — every session loaded from disk gets a fresh
//! 10-second grace window before it can be swept, giving its shim time to
//! reconnect.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use entities::Session;
use hyphae::Gettable;
use myko::{core::item::Eventable, server::CellServerCtx, utils::downcast_item};

/// How long a session must be without a live client before it is DEL'd.
pub const STALE_AFTER: Duration = Duration::from_secs(10);

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
    let mut to_delete: Vec<Arc<str>> = Vec::new();
    let mut still_disconnected: HashSet<Arc<str>> = HashSet::new();

    for (id, item) in session_store.entries().get() {
        let Some(session) = downcast_item::<Session>(&item) else {
            continue;
        };
        let bound_to_live_client = session
            .client_id
            .as_ref()
            .map(|cid| live_client_ids.contains(&cid.0))
            .unwrap_or(false);

        if bound_to_live_client {
            disconnected_since.remove(&id);
            continue;
        }

        still_disconnected.insert(id.clone());
        let first_seen = *disconnected_since
            .entry(id.clone())
            .or_insert(now);
        if now.duration_since(first_seen) >= STALE_AFTER {
            to_delete.push(id);
        }
    }

    // Drop tracking for sessions that no longer exist in the store (e.g.
    // someone DEL'd them out from under us).
    disconnected_since.retain(|id, _| still_disconnected.contains(id));

    for id in to_delete {
        log::info!("[cleanup] DELing session {} (disconnected ≥ {:?})", id, STALE_AFTER);
        if let Err(e) = ctx.del_by_id(Session::ENTITY_NAME_STATIC, &id) {
            log::warn!("[cleanup] del session {} failed: {}", id, e);
            continue;
        }
        disconnected_since.remove(&id);
    }
}
