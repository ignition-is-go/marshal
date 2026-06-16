//! Detect whether THIS session can actually *receive* live peer messages —
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
//! memory. The shim resolves this once at startup and writes a per-session
//! marker file; the `statusline` subcommand reads the marker and renders a
//! `RECV-OFF` warning so the operator knows live receive is not possible.

use std::path::{Path, PathBuf};

/// True iff `parent_cmdline` shows marshal channels were loaded. Pure, so the
/// matching contract is unit-testable without a parent process.
pub fn channels_enabled_in(parent_cmdline: &[String]) -> bool {
    let joined = parent_cmdline.join(" ");
    joined.contains("--dangerously-load-development-channels") && joined.contains("marshal")
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(p) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(p));
        }
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Shim startup: detect this session's receive capability from the parent
/// cmdline and write the marker. Best-effort; never fatal.
pub fn record(session_id: &str) {
    let Some(home) = home_dir() else { return };
    let enabled = channels_enabled_in(&parent_cmdline());
    if !enabled {
        log::warn!(
            "[marshal-shim] channels NOT loaded for this session — live peer \
             messages cannot be received (launch needs \
             --dangerously-load-development-channels server:marshal). statusline \
             will show RECV-OFF."
        );
    }
    write_marker(&home, session_id, enabled);
}

/// statusline: true when this session is confirmed unable to receive live
/// messages. Unknown (no marker) is treated as not-a-warning to avoid false
/// alarms on a session whose shim hasn't recorded yet.
pub fn recv_off(session_id: &str) -> bool {
    home_dir()
        .and_then(|h| read_marker(&h, session_id))
        .is_some_and(|enabled| !enabled)
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

fn marker_path(home: &Path, session_id: &str) -> PathBuf {
    config_dir(home).join("channels").join(session_id)
}

/// Record this session's receive capability where the statusline can read it.
/// Best-effort: failures are logged, never fatal.
pub fn write_marker(home: &Path, session_id: &str, enabled: bool) {
    let path = marker_path(home, session_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, if enabled { "1" } else { "0" }) {
        log::warn!("[marshal-shim] could not write channels marker {path:?}: {e}");
    }
}

/// Read a session's receive capability. `None` = unknown (no marker yet);
/// `Some(false)` = channels confirmed off (show the warning).
pub fn read_marker(home: &Path, session_id: &str) -> Option<bool> {
    let s = std::fs::read_to_string(marker_path(home, session_id)).ok()?;
    match s.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
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
/// (`Get-CimInstance Win32_Process`) rather than PEB memory reads. Run once at
/// startup, off the hot path, so the PowerShell spawn cost is irrelevant.
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
