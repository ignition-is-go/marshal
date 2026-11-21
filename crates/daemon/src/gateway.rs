//! Myko listeners on loopback for a local reverse proxy.

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result, ensure};
use myko_auth::MykoAuthProvider;
use myko_server::{GatewayAccess, MykoServer, router::route_connection_with_auth};
use tokio::net::TcpListener;

/// Start after Myko has replayed the coordination store. The local proxy owns
/// TLS and the network boundary for this listener.
pub fn start(
    address: SocketAddr,
    server: &MykoServer,
    access: GatewayAccess,
    provider: Arc<MykoAuthProvider>,
) -> Result<()> {
    ensure!(
        address.ip().is_loopback(),
        "Marshal proxy listeners must bind to loopback"
    );
    let listener = std::net::TcpListener::bind(address)
        .with_context(|| format!("bind Myko proxy listener at {address}"))?;
    listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(listener)?;
    let context = Arc::new(server.ctx());
    let info = server.server_info();

    tokio::spawn(async move {
        loop {
            let (connection, peer) = match listener.accept().await {
                Ok(connection) => connection,
                Err(error) => {
                    log::error!("Myko proxy listener {address} stopped: {error}");
                    break;
                }
            };
            let context = context.clone();
            let info = info.clone();
            let provider = provider.clone();
            tokio::spawn(async move {
                if let Err(error) =
                    route_connection_with_auth(connection, peer, context, info, access, provider)
                        .await
                {
                    log::debug!("Myko proxy connection at {address} ended: {error}");
                }
            });
        }
    });
    log::info!("marshal {access:?} Myko proxy listener on ws://{address}");
    Ok(())
}
