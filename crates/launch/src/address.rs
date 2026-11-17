//! The launch address: where a launched agent finds the daemon.
//!
//! On the launching machine the daemon is usually `ws://localhost:6155`,
//! which is wrong anywhere else. So the launcher passes a separate launch
//! address, and refuses a loopback one for any adapter but `tmux`, which runs
//! on this machine. The address must reach both of the daemon's listeners:
//! the Myko port it names, and the hook port (6156) on the same host, which
//! the agent's hooks use to register its row.

use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};

/// The daemon address when nothing else names one.
pub const DEFAULT_DAEMON_ADDRESS: &str = "ws://localhost:6155";
/// The hook listener's port (`MARSHAL_HOOK_BIND`); the shim derives it from the
/// daemon address's host.
pub const HOOK_PORT: u16 = 6156;
/// The only adapter allowed a loopback launch address.
pub const LOCAL_ADAPTER: &str = "tmux";

/// The launch address, first found of: `--daemon`, the `launch-address` config
/// file, the `daemon-address` config file, `MARSHAL_DAEMON_ADDRESS`,
/// `MYKO_ADDRESS`, then localhost. The daemon-address order is the shim's.
pub fn resolve(
    flag: Option<String>,
    launch_file: Option<String>,
    daemon_file: Option<String>,
    env: Option<String>,
    legacy_env: Option<String>,
) -> String {
    [flag, launch_file, daemon_file, env, legacy_env]
        .into_iter()
        .flatten()
        .map(|a| a.trim().to_string())
        .find(|a| !a.is_empty())
        .unwrap_or_else(|| DEFAULT_DAEMON_ADDRESS.to_string())
}

/// [`resolve`] from this process's flag, config files and environment.
pub fn from_environment(flag: Option<String>) -> String {
    resolve(
        flag,
        read_config("launch-address"),
        read_config("daemon-address"),
        std::env::var("MARSHAL_DAEMON_ADDRESS").ok(),
        std::env::var("MYKO_ADDRESS").ok(),
    )
}

/// The first non-empty line of `marshal/<name>` in the first config dir that has it.
fn read_config(name: &str) -> Option<String> {
    read_config_in(&config_dirs(), name)
}

/// The first non-empty line of `marshal/<name>` in the first of `dirs` that has it.
pub fn read_config_in(dirs: &[PathBuf], name: &str) -> Option<String> {
    dirs.iter()
        .filter_map(|d| std::fs::read_to_string(d.join("marshal").join(name)).ok())
        .map(|s| s.lines().next().unwrap_or("").trim().to_string())
        .find(|s| !s.is_empty())
}

fn config_dirs() -> Vec<PathBuf> {
    config_dirs_from(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// The shim's config dirs: `$XDG_CONFIG_HOME`, then `~/.config`.
pub fn config_dirs_from(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(x) = xdg.filter(|x| !x.as_os_str().is_empty()) {
        dirs.push(x);
    }
    if let Some(h) = home.filter(|h| !h.as_os_str().is_empty()) {
        dirs.push(h.join(".config"));
    }
    dirs
}

/// Refuse a `marshal/daemon-address` file that names another address than the
/// launch address. The shim reads that file ahead of `MARSHAL_DAEMON_ADDRESS`,
/// so it would silently send the agent to another daemon.
pub fn check_daemon_address_file(dirs: &[PathBuf], launch_address: &str) -> Result<()> {
    for d in dirs {
        let path = d.join("marshal").join("daemon-address");
        let Ok(s) = std::fs::read_to_string(&path) else {
            continue;
        };
        let line = s.lines().next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.trim_end_matches('/') != launch_address.trim().trim_end_matches('/') {
            bail!(
                "{} names {line}, and the shim reads it ahead of the launch address {launch_address}, \
                 so the agent would join another daemon. Nothing was started. Launch with \
                 --daemon {line}, or change that file",
                path.display()
            );
        }
        // The shim takes the first file it finds, as this does.
        return Ok(());
    }
    Ok(())
}

/// The host and port of a `ws://` or `wss://` address. The port defaults to
/// 80 or 443 by scheme; IPv6 hosts keep their brackets off.
pub fn host_port(address: &str) -> Result<(String, u16)> {
    let (rest, default_port) = if let Some(r) = address.strip_prefix("ws://") {
        (r, 80)
    } else if let Some(r) = address.strip_prefix("wss://") {
        (r, 443)
    } else {
        bail!("{address:?} isn't a ws:// or wss:// address");
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = if let Some(r) = authority.strip_prefix('[') {
        let Some((host, after)) = r.split_once(']') else {
            bail!("{address:?} has an unclosed [ in its host");
        };
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        bail!("{address:?} names no host");
    }
    let port = match port {
        Some(p) => p
            .parse()
            .map_err(|_| anyhow::anyhow!("{address:?} has a bad port {p:?}"))?,
        None => default_port,
    };
    Ok((host.to_string(), port))
}

/// Whether `host` is local: `localhost` (and `*.localhost`), 127.0.0.0/8, ::1, or the
/// unspecified 0.0.0.0 and ::, which a remote agent dialling would reach itself with.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    match h.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback() || v4.is_unspecified(),
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_unspecified())
        }
        Err(_) => false,
    }
}

/// Refuse a loopback launch address for any adapter but `tmux`, before
/// anything starts: an agent elsewhere would reach its own machine, not the
/// daemon, and the launch would only time out.
pub fn check_for_adapter(adapter: &str, address: &str) -> Result<()> {
    let (host, _) = host_port(address)?;
    if adapter != LOCAL_ADAPTER && is_loopback_host(&host) {
        bail!(
            "the launch address {address} is loopback, which only reaches this machine, so the \
             {adapter} adapter wasn't run and nothing was started. Pass --daemon <url> or write \
             marshal/launch-address with an address the target can reach on ports {} and {HOOK_PORT}",
            host_port(address).map(|(_, p)| p).unwrap_or(6155)
        );
    }
    Ok(())
}

/// Refuse a loopback hook base (`MARSHAL_BASE_URL`) for any adapter but `tmux`,
/// before anything starts, for the same reason as [`check_for_adapter`]: the
/// agent's hooks would dial its own machine and the agent would never join.
pub fn check_hook_base_for_adapter(adapter: &str, hook_base: Option<&str>) -> Result<()> {
    let Some(base) = hook_base.map(str::trim).filter(|b| !b.is_empty()) else {
        return Ok(());
    };
    let (host, _) = http_host_port(base)?;
    if adapter != LOCAL_ADAPTER && is_loopback_host(&host) {
        bail!(
            "MARSHAL_BASE_URL {base} is loopback, which only reaches this machine, so the \
             {adapter} adapter wasn't run and nothing was started. Set it to the daemon's hook \
             listener at an address the target can reach, or unset it to use port {HOOK_PORT} \
             on the launch address's host"
        );
    }
    Ok(())
}

/// Connect to both of the daemon's listeners at `address`, from here, and say
/// which one can't be reached. An agent whose hooks can't reach the hook port
/// never appears on the roster, and that would otherwise show only as a timeout.
///
/// The hook listener is the one the agent's shim will use: `hook_base` (the
/// `MARSHAL_BASE_URL` the agent is given) if set, else port 6156 on the launch
/// address's host, as the shim derives it.
pub fn probe(address: &str, hook_base: Option<&str>, timeout: Duration) -> Result<()> {
    let daemon = host_port(address)?;
    let hook = match hook_base.map(str::trim).filter(|b| !b.is_empty()) {
        Some(base) => http_host_port(base)?,
        None => (daemon.0.clone(), HOOK_PORT),
    };
    for (what, (host, port), bind) in [
        ("daemon", daemon, "MARSHAL_BIND"),
        ("hook", hook, "MARSHAL_HOOK_BIND"),
    ] {
        if !reachable(&host, port, timeout) {
            bail!(
                "the {what} listener at {host}:{port} can't be reached from here, so nothing was \
                 started. The launch address {address} must reach both of the daemon's listeners; \
                 check that the daemon's {bind} listens on an address this host can reach"
            );
        }
    }
    Ok(())
}

/// The host and port of an `http://` or `https://` hook base, such as
/// `http://h:6156/`; the port defaults to 80 or 443 by scheme.
pub fn http_host_port(base: &str) -> Result<(String, u16)> {
    let (rest, scheme) = if let Some(r) = base.strip_prefix("http://") {
        (r, "ws")
    } else if let Some(r) = base.strip_prefix("https://") {
        (r, "wss")
    } else {
        bail!("the hook base {base:?} isn't an http:// or https:// URL");
    };
    host_port(&format!("{scheme}://{rest}"))
}

fn reachable(host: &str, port: u16, timeout: Duration) -> bool {
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, timeout).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn the_flag_then_the_launch_file_then_the_daemon_address() {
        assert_eq!(
            resolve(
                s("ws://f:1"),
                s("ws://l:1"),
                s("ws://d:1"),
                s("ws://e:1"),
                None
            ),
            "ws://f:1"
        );
        assert_eq!(
            resolve(None, s(" ws://l:1\n"), s("ws://d:1"), None, None),
            "ws://l:1"
        );
        assert_eq!(
            resolve(None, s(""), s("ws://d:1"), s("ws://e:1"), None),
            "ws://d:1"
        );
        assert_eq!(resolve(None, None, None, None, s("ws://m:1")), "ws://m:1");
        assert_eq!(
            resolve(None, None, None, None, None),
            DEFAULT_DAEMON_ADDRESS
        );
    }

    #[test]
    fn only_tmux_may_launch_with_a_loopback_hook_base() {
        for b in [
            "http://localhost:26156",
            "http://127.0.0.1:26156/",
            "http://[::1]:26156",
            "http://0.0.0.0:6156",
        ] {
            assert!(check_hook_base_for_adapter("tmux", Some(b)).is_ok(), "{b}");
            let err = check_hook_base_for_adapter("ssh", Some(b))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("MARSHAL_BASE_URL") && err.contains("nothing was started"),
                "{err}"
            );
        }
        assert!(check_hook_base_for_adapter("ssh", Some("http://192.0.2.1:26156")).is_ok());
        assert!(check_hook_base_for_adapter("ssh", None).is_ok());
        assert!(check_hook_base_for_adapter("ssh", Some(" ")).is_ok());
        assert!(check_hook_base_for_adapter("ssh", Some("ws://h:1")).is_err());
    }

    #[test]
    fn a_hook_base_gives_its_host_and_port() {
        assert_eq!(http_host_port("http://h:7156").unwrap(), ("h".into(), 7156));
        assert_eq!(
            http_host_port("http://h:7156/").unwrap(),
            ("h".into(), 7156)
        );
        assert_eq!(
            http_host_port("http://[::1]:7156").unwrap(),
            ("::1".into(), 7156)
        );
        assert_eq!(http_host_port("http://h").unwrap(), ("h".into(), 80));
        assert_eq!(http_host_port("https://h/x").unwrap(), ("h".into(), 443));
        assert!(http_host_port("ws://h:1").is_err());
    }

    #[test]
    fn host_and_port_come_out_of_an_address() {
        assert_eq!(
            host_port("ws://marshal.mesh:6155").unwrap(),
            ("marshal.mesh".into(), 6155)
        );
        assert_eq!(
            host_port("ws://[::1]:6155/x").unwrap(),
            ("::1".into(), 6155)
        );
        assert_eq!(host_port("wss://h").unwrap(), ("h".into(), 443));
        assert!(host_port("http://h:1").is_err());
        assert!(host_port("ws://:1").is_err());
        assert!(host_port("ws://h:x").is_err());
    }

    #[test]
    fn loopback_is_localhost_127_8_and_v6_one() {
        for h in [
            "localhost",
            "LOCALHOST.",
            "a.localhost",
            "127.0.0.1",
            "127.9.8.7",
            "::1",
            "::ffff:127.0.0.1",
            "0.0.0.0",
            "::",
            "::ffff:0.0.0.0",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in [
            "marshal.mesh",
            "10.0.0.1",
            "100.64.0.2",
            "::2",
            "localhostx",
            "128.0.0.1",
        ] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    #[test]
    fn only_tmux_may_launch_with_a_loopback_address() {
        for a in [
            "ws://localhost:6155",
            "ws://127.0.0.1:6155",
            "ws://[::1]:6155",
            "ws://0.0.0.0:6155",
            "ws://[::]:6155",
        ] {
            assert!(check_for_adapter("tmux", a).is_ok(), "{a}");
            let err = check_for_adapter("ssh", a).unwrap_err().to_string();
            assert!(
                err.contains("loopback") && err.contains("nothing was started"),
                "{err}"
            );
        }
        assert!(check_for_adapter("ssh", "ws://marshal.mesh:6155").is_ok());
        assert!(check_for_adapter("ssh", "nonsense").is_err());
    }

    #[test]
    fn a_closed_listener_is_named_not_timed_out() {
        let daemon = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let hook = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("ws://127.0.0.1:{}", daemon.local_addr().unwrap().port());
        let hook_port = hook.local_addr().unwrap().port();
        let t = Duration::from_millis(500);
        let base = format!("http://127.0.0.1:{hook_port}/");
        probe(&address, Some(&base), t).unwrap();
        drop(hook);
        let err = probe(&address, Some(&base), t).unwrap_err().to_string();
        assert!(
            err.contains("hook listener") && err.contains("MARSHAL_HOOK_BIND"),
            "{err}"
        );
        drop(daemon);
        let err = probe(&address, Some(&base), t).unwrap_err().to_string();
        assert!(
            err.contains("daemon listener") && err.contains("MARSHAL_BIND"),
            "{err}"
        );
    }
}
