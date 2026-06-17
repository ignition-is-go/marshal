//! Detect whether a session can actually *receive* live peer messages —
//! i.e. whether the parent `claude` was launched with
//! `--dangerously-load-development-channels …marshal…`.
//!
//! Why this is needed: there is no in-band MCP signal for it. Claude Code
//! never advertises a channel capability back to the server, and
//! `notifications/claude/channel` is fire-and-forget — if the session didn't
//! load us as a channel, our pushes are dropped silently with no error. So a
//! `send_message` can report "delivered" while the recipient sees nothing.
//!
//! The one reliable source is the parent's command line (the flag is in its
//! argv). We read it via supported OS facilities — `/proc/<ppid>/cmdline` on
//! Linux, `Get-CimInstance Win32_Process` on Windows — NOT by poking process
//! memory.
//!
//! ## Detect LIVE, don't trust a startup marker
//!
//! An earlier design had the shim detect once at startup and write a
//! per-session marker the statusline read back. That raced on **resume**:
//! when Claude resumes/forks a session it relaunches itself through its
//! `--bg-pty-host` session manager, and the shim — spawned in the middle of
//! that relaunch — read a not-yet-settled parent and recorded a false
//! "channels-off", so the statusline showed `RECV-OFF` even though the flag
//! was present. A manual `/mcp reconnect marshal` "fixed" it only because the
//! re-spawned shim detected against the now-settled parent.
//!
//! The statusline renders *continuously, after* the relaunch has settled, so
//! its parent always carries the real flag state. Detecting LIVE off the
//! statusline's own parent at render time is therefore race-free and needs no
//! reconnect. On Windows the parent-cmdline read shells out, so the result is
//! cached per `(ppid, start-time)` — stable for a process's lifetime,
//! invalidated on pid recycle — to keep the per-render hot path cheap.

use std::path::{Path, PathBuf};

/// True iff `parent_cmdline` shows marshal channels were loaded. Pure, so the
/// matching contract is unit-testable without a parent process.
pub fn channels_enabled_in(parent_cmdline: &[String]) -> bool {
    let joined = parent_cmdline.join(" ");
    joined.contains("--dangerously-load-development-channels") && joined.contains("marshal")
}

/// Detect this process's channel-receive capability from its PARENT's command
/// line. `None` = the parent cmdline couldn't be read (treat as unknown — no
/// warning, and a legacy/None Session.channels_enabled); `Some(true|false)` =
/// channels confirmed on|off. Called by the shim at startup (to report
/// `channels_enabled` to the daemon) and, indirectly via [`recv_off_live`], by
/// the statusline on every render.
pub fn detect() -> Option<bool> {
    let cmd = parent_cmdline();
    if cmd.is_empty() {
        return None;
    }
    Some(channels_enabled_in(&cmd))
}

/// statusline: true iff this session is CONFIRMED unable to receive live peer
/// messages, detected LIVE from the parent at render time (see module docs for
/// why live, not a marker). Unknown (parent unreadable) → `false` so we never
/// false-alarm. On Windows the cmdline read is cached per parent identity to
/// keep the per-render path off the PowerShell spawn.
pub fn recv_off_live() -> bool {
    matches!(detect_cached(), Some(false))
}

/// [`detect`] with a per-parent cache. The cache key is the parent's
/// `(ppid, start-time)`: a hit means the same parent process we already
/// inspected, so its (immutable) cmdline flag state still holds. A miss (new
/// or recycled pid) re-reads the cmdline and refreshes the cache.
fn detect_cached() -> Option<bool> {
    let Some((ppid, token)) = super::session_discovery::parent_identity() else {
        // No stable parent identity → can't cache; detect live.
        return detect();
    };
    if let Some(home) = home_dir() {
        if let Some(cached) = read_cache(&home, ppid, token) {
            return Some(cached);
        }
        let detected = detect();
        if let Some(flag) = detected {
            write_cache(&home, ppid, token, flag);
        }
        return detected;
    }
    detect()
}

/// Per-user marshal config dir (mirrors the daemon-address file's resolution).
fn config_dir(home: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA").filter(|s| !s.is_empty()) {
            return PathBuf::from(appdata).join("marshal");
        }
    }
    #[cfg(unix)]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
            return PathBuf::from(xdg).join("marshal");
        }
    }
    home.join(".config").join("marshal")
}

fn cache_path(home: &Path, ppid: u32) -> PathBuf {
    config_dir(home)
        .join("channels-cache")
        .join(ppid.to_string())
}

/// Read the cached flag for `ppid` iff it was recorded against the same
/// `token` (start-time). A mismatch means the pid was recycled — ignore it.
fn read_cache(home: &Path, ppid: u32, token: u128) -> Option<bool> {
    let s = std::fs::read_to_string(cache_path(home, ppid)).ok()?;
    let (tok, flag) = s.trim().split_once(':')?;
    if tok.parse::<u128>().ok()? != token {
        return None;
    }
    match flag {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Cache `flag` for `ppid` stamped with `token`. Best-effort; never fatal.
fn write_cache(home: &Path, ppid: u32, token: u128, flag: bool) {
    let path = cache_path(home, ppid);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = format!("{token}:{}", if flag { "1" } else { "0" });
    if let Err(e) = std::fs::write(&path, body) {
        log::warn!("[marshal-shim] could not write channels cache {path:?}: {e}");
    }
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(not(any(unix, windows)))]
fn home_dir() -> Option<PathBuf> {
    None
}

/// Read the parent process's command line via supported OS facilities.
pub fn parent_cmdline() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        let ppid = unsafe { libc::getppid() } as u32;
        if let Ok(raw) = std::fs::read(format!("/proc/{ppid}/cmdline")) {
            return raw
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
        }
        Vec::new()
    }
    #[cfg(windows)]
    {
        windows_parent_cmdline().unwrap_or_default()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Vec::new()
    }
}

/// Windows: read the parent's CommandLine via the supported WMI query
/// (`Get-CimInstance Win32_Process`) rather than PEB memory reads. The
/// per-parent cache in [`detect_cached`] keeps this off the per-render hot
/// path — it runs at most once per parent process lifetime.
#[cfg(windows)]
fn windows_parent_cmdline() -> Option<Vec<String>> {
    use std::os::windows::process::CommandExt;
    let ppid = super::session_discovery::windows_parent_pid()?;
    // CREATE_NO_WINDOW so no console flashes.
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-CimInstance Win32_Process -Filter \"ProcessId={ppid}\").CommandLine"),
        ])
        .creation_flags(0x08000000)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if line.is_empty() {
        return None;
    }
    // We only ever substring-match the joined cmdline, so a single-element
    // vec carrying the whole line is sufficient.
    Some(vec![line])
}

#[cfg(test)]
mod tests {
    use super::channels_enabled_in;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn flag_with_server_marshal_is_enabled() {
        assert!(channels_enabled_in(&v(&[
            "claude",
            "--dangerously-load-development-channels",
            "server:marshal",
        ])));
    }

    #[test]
    fn flag_with_plugin_marshal_is_enabled() {
        assert!(channels_enabled_in(&v(&[
            "claude",
            "--dangerously-load-development-channels",
            "plugin:marshal-shim@marshal",
        ])));
    }

    #[test]
    fn bare_claude_is_disabled() {
        assert!(!channels_enabled_in(&v(&["claude"])));
    }

    #[test]
    fn flag_for_a_different_server_is_disabled() {
        assert!(!channels_enabled_in(&v(&[
            "claude",
            "--dangerously-load-development-channels",
            "server:something-else",
        ])));
    }

    #[test]
    fn windows_single_line_cmdline_matches() {
        assert!(channels_enabled_in(&v(&[
            "\"C:\\Users\\admin\\.local\\bin\\claude.exe\" --dangerously-load-development-channels server:marshal",
        ])));
    }
}
