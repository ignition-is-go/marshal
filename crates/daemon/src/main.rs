use anyhow::{Context, Result};
use clap::Parser;
use daemon::conn::{handle, AppState};
use daemon::db::Store;
use daemon::paths;
use daemon::state::Roster;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
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

    // Self-replacement detection: capture the binary's mtime at startup.
    // A periodic timer compares it to the current on-disk mtime; if a newer
    // binary has been installed (e.g. via `cargo install`), the daemon exits
    // gracefully so the next shim connect respawns the fresh build.
    let start_mtime = current_exe_mtime();
    let mut staleness_check = tokio::time::interval(Duration::from_secs(5));
    staleness_check.tick().await; // consume the immediate first tick

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
            _ = staleness_check.tick() => {
                if binary_replaced(start_mtime) {
                    info!("binary replaced on disk; exiting so the next connect can respawn");
                    break;
                }
            }
        }
    }
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Read the mtime of the running binary. Returns `None` if anything fails;
/// in that case staleness detection is silently disabled (we'd rather miss a
/// reload than crash on a weird filesystem).
fn current_exe_mtime() -> Option<SystemTime> {
    let path = std::env::current_exe().ok()?;
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Return true if the binary's current on-disk mtime is newer than `start`.
fn binary_replaced(start: Option<SystemTime>) -> bool {
    let start = match start {
        Some(t) => t,
        None => return false,
    };
    let now_mtime = match current_exe_mtime() {
        Some(t) => t,
        None => return false,
    };
    now_mtime > start
}

fn set_socket_perms(p: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(p)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(p, perms)?;
    Ok(())
}
