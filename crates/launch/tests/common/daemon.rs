//! A real `marshal-daemon` for end-to-end tests, from the binary that
//! `MARSHAL_E2E_DAEMON` names (for example upstream's release asset).
//!
//! Self-contained, so another crate's tests can include it with
//! `#[path = "../../launch/tests/common/daemon.rs"] mod daemon;`.
//!
//! The daemon's defaults are 127.0.0.1:6155 and :6156, which on a dev machine
//! may be a live Marshal. So it is never started bare. It always runs with:
//! - `MARSHAL_BIND` and `MARSHAL_HOOK_BIND` on free test ports;
//! - an environment cleared of everything else: no `MYKO_POSTGRES_URL`
//!   (in-memory, keeps nothing), no `MARSHAL_AUTH_*`;
//! - `HOME` and `XDG_*` under cargo's target temp dir.
//!
//! It is killed on drop.

#![allow(dead_code)]

use std::fs::File;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The variable naming the daemon binary. Tests that need it are `#[ignore]`d
/// and skip when it isn't set.
pub const DAEMON_ENV: &str = "MARSHAL_E2E_DAEMON";
const LIVE_PORTS: [u16; 2] = [6155, 6156];
const READY: Duration = Duration::from_secs(20);

pub struct RealDaemon {
    /// `ws://ip:port`, the Myko/WS listener (`MARSHAL_BIND`).
    pub address: String,
    /// `http://ip:port`, the hook listener (`MARSHAL_HOOK_BIND`). Give it to
    /// agents as `MARSHAL_BASE_URL`, since the shim otherwise derives port 6156.
    pub hook_url: String,
    /// The daemon's stdout and stderr.
    pub log: PathBuf,
    /// A scratch folder for this daemon (its HOME), removed on drop.
    pub dir: PathBuf,
    child: Child,
}

/// A free TCP port on `ip`, never one of the live daemon's.
fn free_port(ip: IpAddr) -> u16 {
    loop {
        let port = TcpListener::bind((ip, 0))
            .and_then(|l| l.local_addr())
            .expect("reserve a test port")
            .port();
        if !LIVE_PORTS.contains(&port) {
            return port;
        }
    }
}

fn host_for_url(ip: IpAddr) -> String {
    match ip {
        IpAddr::V6(v6) => format!("[{v6}]"),
        IpAddr::V4(v4) => v4.to_string(),
    }
}

impl RealDaemon {
    /// Start the `MARSHAL_E2E_DAEMON` binary on 127.0.0.1, or `None` if it isn't set.
    pub fn from_env() -> Option<Self> {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        Self::from_env_on(lo, lo)
    }

    /// Start the `MARSHAL_E2E_DAEMON` binary with its WS listener on `ip` and
    /// its hook listener on `hook_ip` (`0.0.0.0` exposes one; a daemon port on a
    /// reachable IP with the hook on 127.0.0.1 is the hook-on-loopback case),
    /// or `None` if it isn't set.
    pub fn from_env_on(ip: IpAddr, hook_ip: IpAddr) -> Option<Self> {
        let bin = std::env::var_os(DAEMON_ENV).filter(|b| !b.is_empty())?;
        let bin = PathBuf::from(bin);
        assert!(bin.is_file(), "{DAEMON_ENV}={} isn't a file", bin.display());

        let port = free_port(ip);
        let hook_port = loop {
            let p = free_port(hook_ip);
            if p != port {
                break p;
            }
        };
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("real-daemon-{}-{port}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("daemon scratch dir");
        let log = dir.join("daemon.log");
        let out = File::create(&log).expect("daemon log");
        let err = out.try_clone().expect("daemon log");

        let bind = SocketAddr::new(ip, port);
        let hook_bind = SocketAddr::new(hook_ip, hook_port);
        assert!(!LIVE_PORTS.contains(&bind.port()) && !LIVE_PORTS.contains(&hook_bind.port()));
        let child = Command::new(&bin)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &dir)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("RUST_LOG", "info")
            .env("MARSHAL_BIND", bind.to_string())
            .env("MARSHAL_HOOK_BIND", hook_bind.to_string())
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .unwrap_or_else(|e| panic!("starting {}: {e}", bin.display()));

        // A connectable address for each bind, 127.0.0.1 for a wildcard one.
        let dial = |ip: IpAddr| -> IpAddr {
            if ip.is_unspecified() {
                "127.0.0.1".parse().unwrap()
            } else {
                ip
            }
        };
        let mut daemon = Self {
            address: format!("ws://{}:{port}", host_for_url(ip)),
            hook_url: format!("http://{}:{hook_port}", host_for_url(hook_ip)),
            log,
            dir,
            child,
        };
        let started = Instant::now();
        for (at, p) in [(dial(ip), port), (dial(hook_ip), hook_port)] {
            while TcpStream::connect_timeout(&SocketAddr::new(at, p), Duration::from_millis(200))
                .is_err()
            {
                if let Ok(Some(status)) = daemon.child.try_wait() {
                    panic!(
                        "marshal-daemon exited ({status}) before listening:\n{}",
                        daemon.log_text()
                    );
                }
                assert!(
                    started.elapsed() < READY,
                    "marshal-daemon didn't listen on {at}:{p} within {READY:?}:\n{}",
                    daemon.log_text()
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Some(daemon)
    }

    /// The daemon's log so far.
    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The WS address with `host` in place of the bind IP, for a wildcard bind.
    pub fn address_via(&self, host: &str) -> String {
        let port = self.address.rsplit(':').next().unwrap_or_default();
        format!("ws://{host}:{port}")
    }

    /// The hook URL with `host` in place of the bind IP, for a wildcard bind.
    pub fn hook_url_via(&self, host: &str) -> String {
        let port = self.hook_url.rsplit(':').next().unwrap_or_default();
        format!("http://{host}:{port}")
    }
}

impl Drop for RealDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}
