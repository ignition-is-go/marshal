//! marshal-daemon — myko coordination server.
//!
//! Single binary: spins up a Myko server over WebSocket and registers
//! the entities defined in the `entities` crate. Persistence is Postgres via
//! myko when `MYKO_POSTGRES_URL` is set (durable, latest-state-per-entity —
//! like pulse-cluster / pulse-ctx); otherwise the daemon runs ephemeral. Bind
//! address is configurable so the server can be hosted remotely; clients
//! (shims, TUIs, web UIs) point their `MykoClient` at it.

use anyhow::{Context, Result};
use myko_server::mcp::dispatch::ServerInfo;
use myko_server::postgres::PostgresConfig;
use myko_server::{BlackholePersister, GatewayAccess, MykoServer};
use std::{
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
};

/// Default MCP/WS bind. Agent coordination trusts the network and
/// its `asSession` is self-asserted, so the service must not be
/// reachable off-box unless deliberately exposed. Production binds the
/// NetBird mesh hostname via `MARSHAL_BIND` — making exposure an explicit
/// act, not a default. Port 6155 is distinct from myko's default 5155.
const DEFAULT_BIND: &str = "127.0.0.1:6155";

/// Default plain-HTTP hook listener bind (the `/hook/*` endpoints). Same
/// loopback-by-default posture; override with `MARSHAL_HOOK_BIND`. A
/// second port because it speaks plain HTTP, not MCP — see `http_listener`.
const DEFAULT_HOOK_BIND: &str = "127.0.0.1:6156";

#[tokio::main]
async fn main() -> Result<()> {
    // Answer `--help` / `--version` before anything starts, and refuse any
    // other argument: configuration comes from the environment only, so an
    // argument here is a mistake, and ignoring it would start a real daemon.
    // `args_os`, so an argument that isn't UTF-8 is refused, not a panic.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match args.first().and_then(|arg| arg.to_str()) {
        Some("-h" | "--help") => {
            print!("{}", usage());
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!("marshal-daemon {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }
    // The daemon always runs in the foreground. `--foreground` stays accepted,
    // alone, because earlier docs told people to pass it, so a unit file
    // written from them keeps working.
    let unknown = match args.first() {
        Some(first) if first.as_os_str() == "--foreground" => args.get(1),
        first => first,
    };
    if let Some(arg) = unknown {
        eprintln!(
            "marshal-daemon: unknown argument: {}",
            arg.to_string_lossy()
        );
        eprintln!("Try 'marshal-daemon --help'.");
        std::process::exit(2);
    }

    init_logging();

    // Resolve via the system resolver (not bare `parse`) so a mesh DNS
    // name like `marshal-01.lucid.host:6155` works and survives the peer's
    // mesh IP being reassigned (next start re-resolves).
    let bind_addr = resolve_bind("MARSHAL_BIND", DEFAULT_BIND)?;
    let hook_bind = resolve_bind("MARSHAL_HOOK_BIND", DEFAULT_HOOK_BIND)?;
    let view_bind = optional_loopback_bind("MARSHAL_VIEW_BIND")?;
    let authenticated_mcp = optional_loopback_bind("MARSHAL_MCP_AUTH_BIND")?
        .map(|address| {
            let required = |key| {
                std::env::var(key)
                    .with_context(|| format!("{key} is required with MARSHAL_MCP_AUTH_BIND"))
            };
            let provider = daemon::auth::authenticated_mcp_provider(
                &required("MARSHAL_MCP_AUTH_ISSUER")?,
                &required("MARSHAL_MCP_AUTH_AUDIENCE")?,
                &required("MARSHAL_MCP_AUTH_RESOURCE")?,
            )?;
            Ok::<_, anyhow::Error>((address, provider))
        })
        .transpose()?;

    // Force-link entities + sagas so their `inventory` registrations aren't
    // dead-code-eliminated.
    marshal_entities::link();
    daemon::link();

    // Client/Server entities are WS-bound and always transient — blackhole them
    // so connection bookkeeping never lands in durable storage (a replayed
    // Client would reference a WS connection that no longer exists).
    let blackhole: Arc<dyn myko::server::Persister> = Arc::new(BlackholePersister);
    // Session is durable but its liveness fields churn on a seconds-scale
    // heartbeat; the filter drops liveness-only SETs from the durable path so
    // the event table grows with session lifecycle, not with heartbeats
    // (lv-6731). Late-bound to the Postgres producer after build below.
    let session_filter = Arc::new(daemon::liveness_filter::SessionLivenessFilter::new());
    let mut builder = MykoServer::builder()
        .with_auth_provider(daemon::auth::trusted_relay_provider())
        .try_after_init(move |server| {
            if let Some(address) = view_bind {
                daemon::gateway::start(
                    address,
                    server,
                    GatewayAccess::ReadOnly,
                    daemon::auth::trusted_relay_provider(),
                )?;
            }
            if let Some((address, provider)) = authenticated_mcp {
                daemon::gateway::start(address, server, GatewayAccess::Full, provider)?;
            }
            Ok(())
        })
        .with_bind_addr(bind_addr)
        .with_persister_override("Client", blackhole.clone())
        .with_persister_override("Server", blackhole.clone())
        // A wait dies with the daemon, so there's nothing to replay, and a held
        // wait's refreshes shouldn't grow the event table.
        .with_persister_override("WakeWatcher", blackhole.clone())
        .with_persister_override("Session", session_filter.clone())
        .with_server_info(marshal_server_info());

    // Persistence: Postgres via myko when `MYKO_POSTGRES_URL` is set — durable,
    // latest-state-per-entity (no append-forever event log and no full-file
    // replay on boot, which the old bespoke JSONL persister had), matching
    // pulse-cluster / pulse-ctx. Without it, run EPHEMERAL (blackhole): the
    // daemon is still usable in dev / CI without a database, and the roster,
    // rooms, and messages regenerate as sessions reconnect. myko-server handles
    // Postgres replay + live-notify internally — no manual replay/watcher here.
    builder = match PostgresConfig::from_env() {
        Some(pg) => {
            log::info!(
                "marshal-daemon persistence: postgres (events table `{}`)",
                pg.table
            );
            builder.with_postgres(pg)
        }
        None => {
            log::warn!(
                "marshal-daemon persistence: EPHEMERAL — no MYKO_POSTGRES_URL set; \
                 roster/rooms/messages will NOT survive a restart"
            );
            builder.with_default_persister(blackhole)
        }
    };

    let server = builder.build();

    // Bind the Session filter to the real durable persister now that the
    // builder has constructed it. In ephemeral mode there is none and the
    // filter keeps dropping, matching the blackhole default.
    if let Some(handle) = server.postgres_producer.clone() {
        session_filter.bind(Arc::new(handle));
    }

    // Spawn the periodic sweeper. WS-shim sessions reap on client loss +
    // grace; pull/hook sessions reap on the activity backstop. See cleanup.
    tokio::spawn(daemon::cleanup::run_sweeper(server.ctx()));

    // Spawn marshal's own plain-HTTP listener for the Claude Code `/hook/*`
    // endpoints. Shares the server ctx (same registry + event log); stock
    // myko's MCP listener is left untouched on its own port.
    //
    // Set on SIGTERM or Ctrl-C, so held waits end cleanly before the daemon exits.
    // A listener that can't bind or stops is logged, and the daemon keeps
    // running, as it always has: the hooks fail quietly within their timeout
    // when nothing answers.
    let launches = std::env::var_os("MARSHAL_LAUNCH_STATE")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("MARSHAL_STATE_DIR")
                .map(|path| std::path::PathBuf::from(path).join("launches"))
        })
        .or_else(|| {
            marshal_launch::record::default_path()
                .ok()
                .map(|p| p.with_file_name("launches"))
        })
        .map(|state| {
            let local = if bind_addr.ip().is_unspecified() {
                let ip = if bind_addr.is_ipv4() {
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                } else {
                    std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                };
                SocketAddr::new(ip, bind_addr.port())
            } else {
                bind_addr
            };
            let local = format!("ws://{local}");
            let launch_address = marshal_launch::address::configured_from_environment(
                std::env::var("MARSHAL_LAUNCH_ADDRESS").ok(),
            )
            .unwrap_or_else(|| local.clone());
            let hook_base = std::env::var("MARSHAL_BASE_URL")
                .ok()
                .filter(|value| !value.is_empty())
                .or_else(|| marshal_launch::api::hook_url(&launch_address, hook_bind.port()).ok());
            Arc::new(marshal_launch::api::Service::new(
                state,
                local,
                launch_address,
                hook_base,
            ))
        });
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    {
        let hook_ctx = Arc::new(server.ctx());
        tokio::spawn(async move {
            if let Err(e) =
                daemon::http_listener::run_with_launch(hook_bind, hook_ctx, stop_rx, launches).await
            {
                log::error!("[hook] listener exited: {e}");
            }
        });
    }

    log::info!("marshal-daemon listening on ws://{bind_addr}, hooks on http://{hook_bind}");
    tokio::select! {
        r = server.run() => r.map_err(|e| anyhow::anyhow!(e))?,
        signal = stop_signal() => {
            log::info!("marshal-daemon: {signal}; ending held waits, then stopping");
            let _ = stop_tx.send(true);
            // Long enough for each held wait to write its reply.
            tokio::time::sleep(STOP_DRAIN).await;
        }
    }
    Ok(())
}
/// How long a stopping daemon gives held waits to write their replies.
const STOP_DRAIN: std::time::Duration = std::time::Duration::from_millis(500);

/// Waits for SIGTERM, which systemd sends on stop and restart, or Ctrl-C.
async fn stop_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = tokio::signal::ctrl_c() => "Ctrl-C",
            },
            Err(e) => {
                log::warn!("marshal-daemon: can't watch for SIGTERM: {e}");
                let _ = tokio::signal::ctrl_c().await;
                "Ctrl-C"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}

/// The `--help` text. The daemon takes no other arguments; it reads its
/// configuration from the environment.
fn usage() -> String {
    format!(
        "marshal-daemon {version}
Marshal coordination daemon: serves the session roster, rooms and messages
to marshal-shim, marshal-tui and the agent plugins.

Usage: marshal-daemon [OPTIONS]

Options:
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit

Environment:
  MARSHAL_BIND       WebSocket and MCP listen address (default {DEFAULT_BIND})
  MARSHAL_HOOK_BIND  HTTP hook listen address (default {DEFAULT_HOOK_BIND})
  MARSHAL_VIEW_BIND  Optional loopback address for read-only Myko WebSocket
  MARSHAL_MCP_AUTH_BIND  Optional loopback address for authenticated MCP
  MARSHAL_MCP_AUTH_ISSUER  Trusted Auth0 issuer
  MARSHAL_MCP_AUTH_AUDIENCE  Expected access-token audience
  MARSHAL_MCP_AUTH_RESOURCE  Canonical HTTPS MCP resource URL
  MYKO_POSTGRES_URL  Postgres URL for durable state; without it the daemon
                     runs ephemeral and keeps nothing across a restart
  RUST_LOG           Log filter (default info)
",
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// Resolve a bind spec from `env_var` (or `default`) via the system
/// resolver, so hostnames — not just numeric IPs — are accepted.
fn optional_loopback_bind(key: &str) -> Result<Option<SocketAddr>> {
    let address: SocketAddr = match std::env::var(key) {
        Ok(value) => value.parse().with_context(|| format!("invalid {key}"))?,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        address.ip().is_loopback(),
        "{key} must be a loopback address"
    );
    Ok(Some(address))
}

fn resolve_bind(env_var: &str, default: &str) -> Result<SocketAddr> {
    let spec = std::env::var(env_var).unwrap_or_else(|_| default.to_string());
    spec.to_socket_addrs()
        .with_context(|| format!("resolving {env_var} '{spec}'"))?
        .next()
        .with_context(|| format!("{env_var} '{spec}' resolved to no addresses"))
}

/// MCP `ServerInfo` for the `/myko/mcp` initialize response.
///
/// marshal runs on *stock* myko 4.24, whose MCP surface is `search` +
/// `execute` (no per-op tool carries a discoverable arg schema). So this
/// blob is load-bearing: it's the operation-id + arg reference the model
/// needs to build `execute` calls, not just flavour text.
///
/// Only a client that speaks to `/myko/mcp` DIRECTLY reads this. Fleet
/// agents go through the marshal-shim, which exposes its own curated tool
/// surface (send_message/broadcast/…) with its own instructions.
fn marshal_server_info() -> ServerInfo {
    ServerInfo {
        name: "marshal".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        instructions: Some(format!(
            "Marshal coordinates sibling coding-agent sessions on this fleet.\n\
             \n\
             This is stock myko 4.24: the MCP surface is `search` (discover operations) + \
             `execute` (run JS — `await myko.command(id, args)` for writes, \
             `await myko.query(id, args)` for reads). The operation ids and args below are \
             what you pass to `execute`.\n\
             \n\
             Your own marshal session_id is injected at session start (in a \
             <marshal_session> block). This HTTP-MCP transport carries no connection \
             identity, so pass it as the `asSession` arg on every command below so peers \
             know who sent the message.\n\
             \n\
             READS (queries):\n\
             - GetAllSessions → the roster: every live session — its id, host, operator, \
             cwd, project, current_task. Recipient ids come from here (session_id uuids). \
             Compose a human label from `host.name` + cwd basename when displaying.\n\
             - GetAllRooms → every room and its id.\n\
             \n\
             WRITES (commands) — args are camelCase JSON:\n\
             - SendMessage      {{ toSessionId, body, asSession }}\n\
             - BroadcastMessage {{ toRoomId, body, asSession }}\n\
             - JoinRoom         {{ name, description?, asSession }}\n\
             - LeaveRoom        {{ room, asSession }}   (room = id or name)\n\
             - AckMessages      {{ messageIds, asSession }}   (rarely needed: inbound \
             messages are auto-acked when surfaced to you at turn start)\n\
             e.g. execute `await myko.command('SendMessage', {{toSessionId, body, asSession}})`.\n\
             \n\
             Inbound peer messages are delivered to you automatically at the start of each \
             turn (a <marshal_inbox> block), pulled by a session hook — you do not poll. \
             {coordination}",
            coordination = marshal_entities::COORDINATION_GUIDANCE,
        )),
        // 4.24 tool-search index — built from the registered operations
        // (no I/O); powers the `search` MCP tool.
        operation_index: Arc::new(myko::operation_index::build_operation_index()),
    }
}

fn init_logging() {
    let mut b = env_logger::Builder::from_default_env();
    if std::env::var("RUST_LOG").is_err() {
        b.filter_level(log::LevelFilter::Info);
    }
    b.init();
}
