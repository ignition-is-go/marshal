use anyhow::{Context, Result};
use clap::Parser;
use daemon::conn::{handle, AppState};
use daemon::db::Store;
use daemon::paths;
use daemon::state::Roster;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::signal;
use tokio::signal::unix::{signal as unix_signal, SignalKind};
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(name = "claude-coord-daemon")]
struct Args {
    /// Run attached to the terminal (also logs to stderr) instead of detaching.
    #[arg(long)]
    foreground: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _log_guard = daemon::log::init(args.foreground)?;

    let socket = paths::socket_path();
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if socket.exists() {
        // Try a connection first; if it succeeds, another daemon is already running.
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            error!("daemon already running at {:?}", socket);
            std::process::exit(1);
        }
        std::fs::remove_file(&socket).ok();
    }
    let listener = UnixListener::bind(&socket).context("binding socket")?;
    set_socket_perms(&socket)?;
    info!(socket = ?socket, "claude-coord-daemon listening");

    std::fs::create_dir_all(paths::state_dir())?;
    let store = Store::open(paths::db_path())?;
    let app = Arc::new(AppState::new(Roster::new(), store));

    let _prune = daemon::prune::spawn(Arc::clone(&app));

    let mut sigterm = unix_signal(SignalKind::terminate()).context("installing SIGTERM handler")?;

    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok((sock, _addr)) => {
                    let app = Arc::clone(&app);
                    tokio::spawn(async move {
                        if let Err(e) = handle(app, sock).await {
                            tracing::warn!(error = %e, "connection ended with error");
                        }
                    });
                }
                Err(e) => { tracing::warn!(error = %e, "accept failed"); }
            },
            _ = signal::ctrl_c() => {
                info!("shutdown requested (SIGINT)");
                break;
            }
            _ = sigterm.recv() => {
                info!("shutdown requested (SIGTERM)");
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

fn set_socket_perms(p: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(p)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(p, perms)?;
    Ok(())
}
