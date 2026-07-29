//! Two concerns live here:
//!
//! 1. [`detect`] — does THIS session have the live channel loaded (parent
//!    launched with `--dangerously-load-development-channels …marshal…`)?
//!    Reported to the daemon on the Session so `SendMessage` can set
//!    `delivered_live` honestly. There is no in-band MCP signal for it, so it
//!    is read from the parent's command line.
//!
//! 2. [`cannot_receive`] — is the daemon UNREACHABLE (the statusline's
//!    top-severity warning, meaning nothing delivers at all)? Delivery has TWO
//!    paths: the live channel (needs the flag) and the turn-boundary INBOX (the
//!    `UserPromptSubmit` hook POSTs to the daemon and surfaces `<marshal_inbox>`
//!    on the NEXT prompt). Neither works if the daemon is unreachable, so that
//!    is the "can't communicate at all" signal, keyed on reachability.
//!
//!    But the two paths are NOT equivalent, and flag-off is NOT harmless: the
//!    inbox only fires at a prompt boundary, so a flag-off session that goes
//!    heads-down (a long autonomous turn) silently misses LIVE messages until it
//!    returns to a prompt. So flag-off is its OWN distinct statusline warning
//!    (`no live channel`), separate from and lower-severity than UNREACHABLE —
//!    surfaced off the shim-written channels file, see `statusline::pick_warnings`.
//!    (The earlier design suppressed it to avoid "false-alarming" flagless VS
//!    Code sessions; that was wrong — the state is real and worth showing.)

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long the statusline's reachability probe waits for a TCP connect. When
/// the daemon is up (the common case) this returns in a few ms on the mesh;
/// the timeout only bites when the daemon is actually down — which is exactly
/// when we want to warn, so a brief render stall then is acceptable.
const REACHABILITY_TIMEOUT: Duration = Duration::from_millis(600);

/// True iff `parent_cmdline` shows marshal channels were loaded. Pure, so the
/// matching contract is unit-testable without a parent process.
pub fn channels_enabled_in(parent_cmdline: &[String]) -> bool {
    let joined = parent_cmdline.join(" ");
    joined.contains("--dangerously-load-development-channels") && joined.contains("marshal")
}

/// Detect this process's LIVE channel capability from its PARENT's command
/// line. `None` = parent cmdline unreadable (unknown → legacy/None Session);
/// `Some(true|false)` = live channel on|off. Used by the shim at startup to
/// report `channels_enabled` to the daemon for `delivered_live` honesty. NOT
/// used for the statusline warning — see [`marshal_reachable`].
pub fn detect() -> Option<bool> {
    let cmd = parent_cmdline();
    if cmd.is_empty() {
        return None;
    }
    Some(channels_enabled_in(&cmd))
}

/// statusline: true iff this session CANNOT receive peer messages at all —
/// i.e. the marshal daemon is unreachable, so neither the live channel nor the
/// inbox can deliver. A flagless-but-daemon-reachable session is NOT warned:
/// its inbox still works. Unknown (no address configured / unresolvable) →
/// `false` so we never false-alarm.
pub fn cannot_receive() -> bool {
    match daemon_address() {
        Some(addr) => !is_reachable(&addr),
        None => false,
    }
}

/// TCP-connect probe of the daemon's `host:port` (parsed from a `ws://…` URL or
/// bare `host:port`). Any resolved address that accepts a connection within the
/// timeout counts as reachable. Resolution/parse failure → `true` (treat as
/// reachable) so a transient DNS hiccup doesn't flash a false warning.
fn is_reachable(addr: &str) -> bool {
    let Some((host, port)) = parse_host_port(addr) else {
        return true;
    };
    match (host.as_str(), port).to_socket_addrs() {
        Ok(socket_addrs) => {
            let mut any = false;
            for sa in socket_addrs {
                any = true;
                if TcpStream::connect_timeout(&sa, REACHABILITY_TIMEOUT).is_ok() {
                    return true;
                }
            }
            // Resolved but nothing accepted → unreachable. If it didn't resolve
            // to anything at all, treat as reachable (don't false-warn).
            !any
        }
        Err(_) => true,
    }
}

/// Parse `host` and `port` from `ws://host:port[/path]`, `wss://…`, or a bare
/// `host:port`. Returns `None` if no port is present.
fn parse_host_port(addr: &str) -> Option<(String, u16)> {
    let no_scheme = addr
        .strip_prefix("ws://")
        .or_else(|| addr.strip_prefix("wss://"))
        .unwrap_or(addr);
    let authority = no_scheme.split(['/', '?', '#']).next().unwrap_or(no_scheme);
    let (host, port) = authority.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?))
}

/// Resolve the daemon WS address the same way the shim does: env first
/// (`MARSHAL_DAEMON_ADDRESS`, legacy `MYKO_ADDRESS`), then the
/// `daemon-address` file the deploy role writes. `None` if nothing is
/// configured.
fn daemon_address() -> Option<String> {
    for var in ["MARSHAL_DAEMON_ADDRESS", "MYKO_ADDRESS"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    let home = home_dir()?;
    let path = config_dir(&home).join("daemon-address");
    let contents = std::fs::read_to_string(path).ok()?;
    let line = contents.lines().next()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
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
/// (`Get-CimInstance Win32_Process`) rather than PEB memory reads. Only called
/// once, at shim startup, off the hot path.
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
    use super::{channels_enabled_in, parse_host_port};

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
    fn parses_ws_url_with_port() {
        assert_eq!(
            parse_host_port("ws://100.86.142.61:6155"),
            Some(("100.86.142.61".to_string(), 6155))
        );
    }

    #[test]
    fn parses_bare_host_port_and_strips_path() {
        assert_eq!(
            parse_host_port("marshal-01.lucid.host:6156/hook"),
            Some(("marshal-01.lucid.host".to_string(), 6156))
        );
    }

    #[test]
    fn no_port_is_none() {
        assert_eq!(parse_host_port("ws://justahost"), None);
    }
}
