//! Watches the running TUI's own binary on disk and re-execs when a new
//! version is installed (e.g. `cargo install --path crates/tui`
//! overwrites `~/.cargo/bin/marshal-tui`).
//!
//! Mirrors `marshal-shim`'s `self_update.rs` — same poll-and-exec
//! mechanism, two TUI-specific differences:
//!
//! 1. **Idle gate.** The shim gates on MCP-server idleness (no in-flight
//!    requests). The TUI doesn't have RPCs; the operator may be
//!    mid-keystroke. We gate on a short keystroke-idle window — long
//!    enough that an actively-typing user doesn't lose focus mid-input
//!    (`KEYSTROKE_IDLE_WINDOW = 500ms`), short enough that the rollout
//!    doesn't stall on someone scrolling through the task list.
//!
//! 2. **Terminal cleanup before exec.** The TUI is in raw mode +
//!    alternate screen + (potentially) mouse capture. Doing `exec()`
//!    without unwinding leaves the new binary inheriting a corrupted
//!    terminal state — double-init in the alt screen, raw mode left
//!    enabled, cursor hidden. `restore_terminal_global` runs the
//!    crossterm teardown directly on `io::stdout()` (no `Terminal`
//!    handle needed) so the watcher can call it from its tokio task
//!    immediately before `exec()`.
//!
//! On any failure we keep running on the old binary and log to stderr;
//! the next mtime bump retries with the cached newer mtime advanced so
//! the same broken binary doesn't loop.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime};

use crossterm::{
    cursor::Show,
    execute,
    terminal::{LeaveAlternateScreen, disable_raw_mode},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const KEYSTROKE_IDLE_WINDOW: Duration = Duration::from_millis(500);
const IDLE_POLL: Duration = Duration::from_millis(100);
const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Tracks the wall-clock millis of the most recent keypress the input
/// loop served. The self-update watcher reads this to decide whether
/// it's safe to swap the binary now or wait another tick — same role
/// the shim's `Activity` plays for MCP traffic, scaled down for an
/// interactive TUI's keystroke cadence.
pub struct KeyActivity {
    last_ms: AtomicI64,
}

impl Default for KeyActivity {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyActivity {
    pub fn new() -> Self {
        Self {
            last_ms: AtomicI64::new(now_ms()),
        }
    }

    /// Bump the last-keypress timestamp. Called from the input handler
    /// every time a `KeyEventKind::Press` arrives.
    pub fn bump(&self) {
        self.last_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// True when the last keypress was at least `dur` ago.
    pub fn idle_for(&self, dur: Duration) -> bool {
        let last = self.last_ms.load(Ordering::Relaxed);
        let elapsed = now_ms().saturating_sub(last);
        elapsed >= dur.as_millis() as i64
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn spawn(activity: Arc<KeyActivity>) {
    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[marshal-tui] cannot resolve current_exe ({e}); auto-restart disabled");
            return;
        }
    };
    let initial_mtime = match read_mtime(&exe_path) {
        Some(t) => t,
        None => {
            log::warn!(
                "[marshal-tui] cannot stat {} for self-update; auto-restart disabled",
                exe_path.display()
            );
            return;
        }
    };
    log::info!(
        "[marshal-tui] self-update watcher polling {} every {:?}",
        exe_path.display(),
        POLL_INTERVAL
    );

    tokio::spawn(async move {
        run(exe_path, initial_mtime, activity).await;
    });
}

async fn run(exe_path: PathBuf, initial_mtime: SystemTime, activity: Arc<KeyActivity>) {
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
            "[marshal-tui] detected updated binary at {}; waiting for keystroke idle",
            exe_path.display()
        );
        wait_for_idle(&activity).await;

        if let Err(e) = smoke_test(&exe_path).await {
            log::warn!(
                "[marshal-tui] smoke test failed for new binary: {e}; staying on old binary"
            );
            continue;
        }

        log::info!("[marshal-tui] tui binary updated, re-execing");
        // Best-effort terminal cleanup so the new process starts on a
        // clean screen — leaving alt-screen restores whatever was
        // visible before the TUI launched, disable_raw_mode lets the
        // shell own the keyboard again, Show makes the cursor visible
        // for the brief window before the new binary re-enters alt
        // screen and re-hides it. Errors here are ignored: if the
        // terminal is already torn down (e.g. user Ctrl-C'd at the
        // exact moment), the new process will still init correctly.
        restore_terminal_global();
        re_exec(&exe_path);
        // re_exec only returns on failure.
    }
}

fn read_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

async fn wait_for_idle(activity: &KeyActivity) {
    while !activity.idle_for(KEYSTROKE_IDLE_WINDOW) {
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

/// Best-effort crossterm cleanup callable without the `Terminal`
/// handle owned by `render_loop`. Mirrors the teardown sequence at
/// the end of `render_loop` (raw-mode off, leave alt screen, show
/// cursor) but writes directly to `io::stdout()` so the watcher's
/// tokio task can call it before `exec()`. Errors are logged and
/// swallowed — a failed cleanup is annoying but not fatal; the new
/// binary's startup will paper over most cases by re-entering raw
/// mode + alt screen.
fn restore_terminal_global() {
    if let Err(e) = disable_raw_mode() {
        log::warn!("[marshal-tui] disable_raw_mode failed pre-exec: {e}");
    }
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, LeaveAlternateScreen, Show) {
        log::warn!("[marshal-tui] terminal restore failed pre-exec: {e}");
    }
}

#[cfg(unix)]
fn re_exec(path: &Path) {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let err = std::process::Command::new(path).args(&args).exec();
    log::error!(
        "[marshal-tui] exec({}) failed: {err}; continuing on old binary",
        path.display()
    );
}

#[cfg(not(unix))]
fn re_exec(path: &Path) {
    log::warn!(
        "[marshal-tui] self-update re-exec is only supported on unix; \
         binary at {} will not auto-restart",
        path.display()
    );
}
