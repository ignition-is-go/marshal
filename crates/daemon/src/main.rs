//! claude-coord-daemon — myko coordination server.
//!
//! Single binary: spins up a myko `CellServer` over WebSocket and registers
//! the entities defined in the `entities` crate. No persistence is wired up
//! — a restart drops everything. Bind address is configurable so the server
//! can be hosted remotely; clients (shims, TUIs, web UIs) point their
//! `MykoClient` at it.

use anyhow::Result;
use myko_server::{BlackholePersister, CellServer};
use std::{net::SocketAddr, sync::Arc};

/// Default bind address. Port 6155 is deliberately distinct from myko's
/// default 5155 — claude-coord may run on the same host as a myko server.
const DEFAULT_BIND: &str = "127.0.0.1:6155";

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let bind_addr: SocketAddr = std::env::var("CLAUDE_COORD_BIND")
        .unwrap_or_else(|_| DEFAULT_BIND.to_string())
        .parse()?;

    // Force-link entities + sagas so their `inventory` registrations aren't
    // dead-code-eliminated.
    entities::link();
    daemon::link();

    let server = CellServer::builder()
        .with_bind_addr(bind_addr)
        .with_default_persister(Arc::new(BlackholePersister))
        .build();

    log::info!("claude-coord-daemon listening on ws://{bind_addr}");
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
