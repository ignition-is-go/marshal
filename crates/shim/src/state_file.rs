//! Per-session state file that maps a Claude Code process to its
//! marshal nickname.
//!
//! Why: an out-of-band consumer (the `marshal-shim statusline`
//! subcommand) needs the human-readable nickname for visual session
//! identification but has no daemon connection of its own. The shim
//! writes a small JSON file the subcommand can read with plain stdlib
//! calls.
//!
//! Key: the parent PID. The shim is launched as a subprocess of
//! Claude Code, and any Claude-Code-spawned sibling (a statusLine
//! subcommand, a hook) shares the same parent. Both sides key by the
//! Claude Code PID to find each other without any extra plumbing.
//!
//! Robustness: the reader walks its ancestor chain (immediate parent,
//! then grandparent, etc.) so a one- or two-level shell wrapper
//! between Claude Code and the renderer doesn't break the lookup —
//! we just keep walking up until we hit the PID that matches what the
//! shim wrote (which is always Claude Code itself).
//!
//! Multi-session-per-cwd: each Claude Code instance has a unique PID,
//! so its shim writes a unique state file; no collision between
//! concurrent sessions in the same directory.
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

/// How far up the ancestor chain `read_nickname` will walk before
/// giving up. Five covers the realistic cases (direct child of Claude
/// Code, or up to four levels of shell wrappers between us and
/// Claude Code) without spending unbounded syscalls if the file
/// genuinely isn't there.
const MAX_ANCESTOR_WALK: usize = 5;

/// Read the nickname for *this* process tree from the PPID-keyed state
/// file. Used by `marshal-shim statusline`.
///
/// Walks the ancestor chain so the lookup survives intermediate shell
/// wrappers — at some ancestor PID we'll hit the Claude Code process,
/// which is the same PID the MCP shim wrote its file under.
pub fn read_nickname() -> Option<String> {
    let dir = state_dir()?;
    let mut pid = current_ppid()?;
    for _ in 0..MAX_ANCESTOR_WALK {
        let path = dir.join(format!("shim-by-ppid-{pid}.json"));
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(nick) = v
                    .get("nickname")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                {
                    return Some(nick.to_string());
                }
            }
        }
        // Stop walking once we hit the init/root sentinel — anything
        // above PID 1 (Linux/macOS) or 0/4 (Windows kernel/system) is
        // not a Claude Code process by definition.
        if pid <= 1 {
            return None;
        }
        pid = parent_of(pid)?;
    }
    None
}

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
    let Ok(bytes) = serde_json::to_vec_pretty(&body) else {
        return;
    };
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
    // process itself, only in the toolhelp snapshot. Reuse the
    // arbitrary-PID lookup with our own PID.
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    let pid = unsafe { GetCurrentProcessId() };
    parent_of(pid)
}

#[cfg(not(any(unix, windows)))]
fn current_ppid() -> Option<u32> {
    None
}

// ─── Platform: parent PID of an arbitrary process ───────────────────────────

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    // procfs has the parent PID one line in:
    //   PPid:\t<num>
    let body = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<u32> {
    // macOS has no /proc and libc 0.2 doesn't expose KERN_PROC_PID /
    // kinfo_proc. `ps -o ppid= -p <pid>` is the cheapest portable
    // way to read a parent PID without pulling in a sysctl FFI shim.
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()?.trim().parse().ok()
}

#[cfg(windows)]
fn parent_of(pid: u32) -> Option<u32> {
    // Toolhelp snapshot is the documented way to read a process's
    // parent on Windows. We walk the snapshot once per call; cheap
    // enough for the small ancestor walk we do here.
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next, TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
        let mut found = None;
        if Process32First(snap, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == pid {
                    found = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32Next(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        found
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn parent_of(_pid: u32) -> Option<u32> {
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
