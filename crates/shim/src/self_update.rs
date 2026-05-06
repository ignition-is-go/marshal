//! Watches the running shim's own binary on disk and re-execs when a new
//! version is installed (e.g. `cargo install --path crates/shim` overwrites
//! `~/.cargo/bin/marshal-shim`).
//!
//! Approach: every `POLL_INTERVAL` we stat `current_exe()` and compare its
//! mtime against the one we recorded at startup. A bump means a new binary
//! is on disk. We then:
//!   1. wait until the MCP server has been idle for `IDLE_WINDOW`,
//!   2. spawn the new binary with `--check` as a smoke test,
//!   3. re-exec with the same argv if the smoke test succeeds.
//!
//! On any failure we keep running on the old binary and log to stderr; the
//! next mtime bump retries.

use crate::activity::Activity;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const IDLE_WINDOW: Duration = Duration::from_secs(1);
const IDLE_POLL: Duration = Duration::from_millis(200);
const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(5);

pub fn spawn(activity: Arc<Activity>) {
    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log::warn!(
                "[marshal-shim] cannot resolve current_exe ({e}); auto-restart disabled"
            );
            return;
        }
    };
    let initial_mtime = match read_mtime(&exe_path) {
        Some(t) => t,
        None => {
            log::warn!(
                "[marshal-shim] cannot stat {} for self-update; auto-restart disabled",
                exe_path.display()
            );
            return;
        }
    };
    log::info!(
        "[marshal-shim] self-update watcher polling {} every {:?}",
        exe_path.display(),
        POLL_INTERVAL
    );

    tokio::spawn(async move {
        run(exe_path, initial_mtime, activity).await;
    });
}

async fn run(exe_path: PathBuf, initial_mtime: SystemTime, activity: Arc<Activity>) {
    let mut last_known = initial_mtime;
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let Some(current) = read_mtime(&exe_path) else {
            continue;
        };
        if current <= last_known {
            continue;
        }
        // Advance our reference even if the smoke test fails — we don't
        // want to re-attempt the same broken binary every poll.
        last_known = current;
        log::info!(
            "[marshal-shim] detected updated binary at {}; waiting for idle",
            exe_path.display()
        );
        wait_for_idle(&activity).await;

        if let Err(e) = smoke_test(&exe_path).await {
            log::warn!(
                "[marshal-shim] smoke test failed for new binary: {e}; staying on old binary"
            );
            continue;
        }

        log::info!("[marshal-shim] shim binary updated, re-execing");
        re_exec(&exe_path);
        // re_exec only returns on failure.
    }
}

fn read_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

async fn wait_for_idle(activity: &Activity) {
    while !activity.idle_for(IDLE_WINDOW) {
        tokio::time::sleep(IDLE_POLL).await;
    }
}

async fn smoke_test(path: &Path) -> std::io::Result<()> {
    let mut cmd = tokio::process::Command::new(path);
    cmd.arg("--check");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    let mut child = cmd.spawn()?;
    let status = match tokio::time::timeout(SMOKE_TEST_TIMEOUT, child.wait()).await {
        Ok(s) => s?,
        Err(_) => {
            let _ = child.kill().await;
            return Err(std::io::Error::other("smoke test timed out"));
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "smoke test exited with {status}"
        )))
    }
}

#[cfg(unix)]
fn re_exec(path: &Path) {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let err = std::process::Command::new(path).args(&args).exec();
    log::error!(
        "[marshal-shim] exec({}) failed: {err}; continuing on old binary",
        path.display()
    );
}

#[cfg(not(unix))]
fn re_exec(path: &Path) {
    log::warn!(
        "[marshal-shim] self-update re-exec is only supported on unix; \
         binary at {} will not auto-restart",
        path.display()
    );
}
