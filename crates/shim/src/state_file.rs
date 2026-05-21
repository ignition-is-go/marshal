//! Per-session state file that maps a Claude Code process to its
//! marshal nickname.
//!
//! Why: an out-of-band consumer (e.g. a `statusLine` script) needs
//! the human-readable nickname for visual session identification but
//! can't speak MCP/WebSocket itself. The shim writes a small JSON
//! file the consumer can read with plain text tools.
//!
//! Key: the parent PID. The shim is launched as a subprocess of
//! Claude Code, and any Claude-Code-spawned helper (statusline
//! script, hook) is a sibling — they share the same parent. Both
//! sides key by their parent's PID to find each other without any
//! extra plumbing.
//!
//! The file is written atomically (tmp + rename) and refreshed
//! whenever the nickname changes upstream — the daemon's dedupe saga
//! may rename us shortly after first connect, so we update on every
//! periodic tick rather than once at startup.
//!
//! Locations:
//! - Linux/macOS: `$XDG_STATE_HOME/marshal/shim-by-ppid-<ppid>.json`
//!   (defaulting to `~/.local/state/marshal/...`).
//! - Windows: `%LOCALAPPDATA%\marshal\state\shim-by-ppid-<ppid>.json`.
//! - Other targets: no-op.

use chrono::Utc;
use marshal_entities::{Session, SessionId};
use serde_json::json;
use std::path::PathBuf;

pub fn write(session: &Session, session_id: &SessionId) {
    let Some(ppid) = current_ppid() else { return };
    let Some(dir) = state_dir() else { return };
    let path = dir.join(format!("shim-by-ppid-{ppid}.json"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::debug!("[marshal-shim] state file mkdir {dir:?} failed: {e}");
        return;
    }
    let body = json!({
        "session_id": session_id.0.as_ref(),
        "nickname": session.nickname,
        "pid": session.pid,
        "ppid": ppid,
        "cwd": session.cwd,
        "git_branch": session.git_branch,
        "updated_at": Utc::now().timestamp_millis(),
    });
    let Ok(bytes) = serde_json::to_vec_pretty(&body) else { return };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        log::debug!("[marshal-shim] state file write {tmp:?} failed: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        log::debug!("[marshal-shim] state file rename {path:?} failed: {e}");
    }
}

// ─── Platform: current parent PID ───────────────────────────────────────────

#[cfg(unix)]
fn current_ppid() -> Option<u32> {
    // POSIX getppid() never fails.
    Some(unsafe { libc::getppid() as u32 })
}

#[cfg(windows)]
fn current_ppid() -> Option<u32> {
    // Windows has no getppid(): the parent PID isn't stored on the
    // process itself, only in the toolhelp snapshot. Walk the
    // snapshot, find our PID, return its parent.
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    unsafe {
        let pid = GetCurrentProcessId();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
        let mut ppid = None;
        if Process32First(snap, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == pid {
                    ppid = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32Next(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        ppid
    }
}

#[cfg(not(any(unix, windows)))]
fn current_ppid() -> Option<u32> {
    None
}

// ─── Platform: state directory ──────────────────────────────────────────────

#[cfg(unix)]
fn state_dir() -> Option<PathBuf> {
    // Prefer XDG_STATE_HOME; fall back to ~/.local/state.
    // (STATE rather than CACHE because the file describes live
    // process identity, not a regenerable cache artifact.)
    if let Some(s) = std::env::var_os("XDG_STATE_HOME") {
        let p = PathBuf::from(s);
        if !p.as_os_str().is_empty() {
            return Some(p.join("marshal"));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/state/marshal"))
}

#[cfg(windows)]
fn state_dir() -> Option<PathBuf> {
    // Per-user, non-roaming. APPDATA (roaming) is the fallback only
    // for the unlikely case LOCALAPPDATA isn't set.
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA"))?;
    Some(PathBuf::from(base).join("marshal").join("state"))
}

#[cfg(not(any(unix, windows)))]
fn state_dir() -> Option<PathBuf> {
    None
}
