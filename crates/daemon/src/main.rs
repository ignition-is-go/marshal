//! marshal-daemon — myko coordination server.
//!
//! Single binary: spins up a myko `CellServer` over WebSocket and registers
//! the entities defined in the `entities` crate. Events are persisted to an
//! append-only JSONL log under `$MARSHAL_STATE_DIR`
//! (default `~/.local/state/marshal/events.jsonl`); on startup the log
//! is replayed into the registry so sessions and messages survive daemon
//! restarts. Bind address is configurable so the server can be hosted
//! remotely; clients (shims, TUIs, web UIs) point their `MykoClient` at it.

use anyhow::{Context, Result};
use daemon::persister::{DiskPersister, default_state_dir, migrate_from_claude_coord};
use myko_server::{BlackholePersister, CellServer};
use std::{net::SocketAddr, sync::Arc};

/// Default bind address. Binds all interfaces by default so peers on
/// other hosts can reach the daemon without the user remembering to
/// override `MARSHAL_BIND`. Port 6155 is deliberately distinct from
/// myko's default 5155 — marshal may run on the same host as a myko
/// server. Restrict by setting `MARSHAL_BIND=127.0.0.1:6155` (or any
/// other interface) explicitly.
const DEFAULT_BIND: &str = "0.0.0.0:6155";

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let bind_addr: SocketAddr = std::env::var("MARSHAL_BIND")
        .unwrap_or_else(|_| DEFAULT_BIND.to_string())
        .parse()?;

    // Force-link entities + sagas so their `inventory` registrations aren't
    // dead-code-eliminated.
    marshal_entities::link();
    daemon::link();

    let state_dir = default_state_dir();
    let log_path = state_dir.join("events.jsonl");
    if let Err(e) = migrate_from_claude_coord(&log_path) {
        log::warn!(
            "[migrate] legacy claude-coord log migration failed: {e} \
             (continuing with empty {})",
            log_path.display(),
        );
    }
    let persister = Arc::new(
        DiskPersister::new(&log_path)
            .with_context(|| format!("opening event log at {}", log_path.display()))?,
    );
    log::info!("marshal-daemon event log: {}", log_path.display());

    // Default = persist to disk. Client/Server entities are WS-bound and
    // intentionally transient — overriding them to Blackhole keeps the log
    // free of connection bookkeeping that would only confuse a restart
    // (replayed Clients reference WS connections that no longer exist).
    let blackhole: Arc<dyn myko::server::Persister> = Arc::new(BlackholePersister);
    let server = CellServer::builder()
        .with_bind_addr(bind_addr)
        .with_default_persister(persister.clone() as Arc<dyn myko::server::Persister>)
        .with_persister_override("Client", blackhole.clone())
        .with_persister_override("Server", blackhole)
        .build();

    // Replay the log into the just-built server before we accept any
    // connection — sagas and entity stores must reflect the on-disk
    // history before clients can race against it.
    let ctx = server.ctx();
    let restored = persister
        .replay(&ctx)
        .with_context(|| format!("replaying event log {}", log_path.display()))?;
    log::info!("marshal-daemon restored {restored} entities from disk");

    // Tail the log so external appends / migrations against a running
    // daemon get picked up live. `_watcher` must be held for the lifetime
    // of the daemon — dropping it stops the notify thread.
    let _watcher = persister
        .start_watcher(server.ctx())
        .with_context(|| format!("starting watcher on {}", log_path.display()))?;

    // Shared SseChannels map: McpSessionMirror inserts per-SSE channel,
    // run_sweeper consults it to keep HTTP-MCP sessions alive, push
    // loop reads it to route peer-message frames.
    let sse_channels = daemon::mcp_observer::SseChannels::new();

    // Spawn the periodic sweeper that DELs sessions whose bound client
    // has been gone for more than `cleanup::STALE_AFTER`. HTTP-MCP
    // sessions are exempt while their SSE stream is open.
    tokio::spawn(daemon::cleanup::run_sweeper(
        server.ctx(),
        sse_channels.clone(),
    ));

    // Register the MCP-session observer so HTTP-connected agents
    // (Claude Code via `"type": "http"` MCP) materialise a `Session`
    // entity in the registry on `initialize`, mirroring what the
    // marshal-shim does on WebSocket connect.
    server.set_mcp_session_observer(daemon::mcp_observer::McpSessionMirror::new(
        Arc::new(server.ctx()),
        sse_channels.clone(),
    ));

    // Push loop: forward new `Message` entities into the SSE streams of
    // HTTP-connected recipients. Shim-connected peers continue to get
    // their notifications via the existing on_command::<NotifyChannel>
    // path; this only touches sessions with an entry in `sse_channels`.
    tokio::spawn(daemon::push::run_push_loop(server.ctx(), sse_channels));

    log::info!("marshal-daemon listening on ws://{bind_addr}");
    server.run().await.map_err(|e| anyhow::anyhow!(e))?;
    Ok(())
}

fn init_logging() {
    let mut b = env_logger::Builder::from_default_env();
    if std::env::var("RUST_LOG").is_err() {
        b.filter_level(log::LevelFilter::Info);
    }
    b.init();
}
