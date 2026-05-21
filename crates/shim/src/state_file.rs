//! Per-session state file that maps a Claude Code process to its
//! marshal nickname.
//!
//! Why: an out-of-band consumer (e.g. a `statusLine` script) needs
//! the human-readable nickname for visual identification but can't
//! speak MCP/WebSocket itself. The shim writes a small JSON file the
//! consumer can read with `cat`/`jq`.
//!
//! Key: the parent PID. The shim is launched as a subprocess of
//! Claude Code, and Claude Code-spawned helpers (statusline scripts,
//! hooks) are siblings — they share the same parent. So both sides
//! key by `getppid()` to find each other without any extra plumbing.
//!
//! The file is written atomically (tmp + rename) and refreshed
//! whenever the nickname changes upstream — the daemon's dedupe saga
//! may rename us shortly after first connect, so we update on every
//! periodic tick rather than once at startup.
//!
//! Non-Unix: no-op. Statusline scripts are bash on macOS/Linux; the
//! file-by-PPID convention has no obvious Windows equivalent and we
//! defer that until a Windows consumer actually exists.

#[cfg(unix)]
mod imp {
    use chrono::Utc;
    use marshal_entities::{Session, SessionId};
    use serde_json::json;
    use std::path::PathBuf;

    pub fn current_ppid() -> u32 {
        // POSIX getppid() never fails.
        unsafe { libc::getppid() as u32 }
    }

    fn state_dir() -> Option<PathBuf> {
        // Prefer XDG_STATE_HOME; fall back to ~/.local/state.
        // (We use STATE rather than CACHE because the file describes
        // live process identity, not a regenerable cache artifact.)
        if let Some(s) = std::env::var_os("XDG_STATE_HOME") {
            let p = PathBuf::from(s);
            if !p.as_os_str().is_empty() {
                return Some(p.join("marshal"));
            }
        }
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".local/state/marshal"))
    }

    fn path_for(ppid: u32) -> Option<PathBuf> {
        Some(state_dir()?.join(format!("shim-by-ppid-{ppid}.json")))
    }

    pub fn write(session: &Session, session_id: &SessionId) {
        let ppid = current_ppid();
        let Some(path) = path_for(ppid) else { return };
        let Some(dir) = path.parent() else { return };
        if let Err(e) = std::fs::create_dir_all(dir) {
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

}

#[cfg(not(unix))]
mod imp {
    use marshal_entities::{Session, SessionId};
    pub fn write(_session: &Session, _session_id: &SessionId) {}
}

pub use imp::write;
