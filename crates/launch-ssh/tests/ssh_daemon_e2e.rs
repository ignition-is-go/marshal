//! The ssh adapter against a real marshal-daemon.
//!
//! `marshal_launch::launch` drives `marshal-launch-ssh` (found on PATH as an
//! external adapter) through `tests/fixtures/fake-ssh`, so the adapter's real
//! remote scripts run on this machine as the "target". The daemon is the
//! binary built from this revision (`MARSHAL_E2E_DAEMON`), in memory, on test ports
//! bound to this machine's non-loopback address, never 6155/6156.
//!
//! The harness is a stub `claude` that does what Claude's hooks do: it runs
//! the real 0.60 `marshal-shim claude-hook session-start` (`MARSHAL_E2E_SHIM`),
//! so the roster row arrives through the hook listener and not from this
//! test, then polls `claude-hook prompt-submit` and keeps what it's handed:
//! the first task. The target's `MARSHAL_BASE_URL` names the test hook port,
//! as a target provisioned for a non-default daemon would set it.
//!
//! A second daemon keeps its hook listener on loopback. The launch fails in
//! the adapter's check, naming MARSHAL_HOOK_BIND, and starts nothing.
//!
//! One test in its own binary: the adapter runs as a child process and
//! inherits this process's environment, which the test sets.
//!
//! ```text
//! MARSHAL_E2E_DAEMON=<target>/debug/marshal-daemon \
//! MARSHAL_E2E_SHIM=<target>/debug/marshal-shim \
//! cargo test -p marshal-launch-ssh --test ssh_daemon_e2e -- --ignored --nocapture
//! ```
//!
//! It is `#[ignore]`d. Run explicitly, it fails unless both variables, a
//! non-loopback address, tmux and python3 or jq are there.

#![cfg(unix)]

#[path = "../../launch/tests/common/daemon.rs"]
mod daemon;

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marshal_launch::{ExternalBackend, LaunchBackend, Plan};

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

fn have(program: &str, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// This machine's address on its default route, if it has one. No packet is sent.
fn lan_ip() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("10.255.255.255:9").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

fn write_exe(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Claude, as far as the launch and its hooks go. `claude plugin list` lists
/// nothing (the target is wired by hand); otherwise it registers through
/// SessionStart, then polls UserPromptSubmit and keeps what it's handed.
const STUB_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = plugin ]; then echo '[]'; exit 0; fi
sid=$(cat /proc/sys/kernel/random/uuid)
echo "$sid" > session-id
hook() {
  printf '{"session_id":"%s","cwd":"%s","hook_event_name":"%s"}' "$sid" "$PWD" "$2" \
    | marshal-shim claude-hook "$1"
}
hook session-start SessionStart > session-start.out 2>&1
while :; do
  hook prompt-submit UserPromptSubmit >> prompt-submit.out 2>&1
  sleep 1
done
"#;

struct Target {
    root: PathBuf,
    /// tmux's socket folder, kept short: a socket path over ~104 bytes
    /// can't be bound, and cargo's target dir can be deep.
    tmux: tempfile::TempDir,
}

impl Target {
    /// A fake "remote" machine: its HOME, its PATH (the stub Claude and the
    /// real shim), Claude wired by hand with the wait hook, and a work folder.
    fn new(shim: &Path) -> Target {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("ssh-daemon-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["bin", "home/.claude", "work"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(shim, root.join("bin/marshal-shim")).unwrap();
        write_exe(&root.join("bin/claude"), STUB_CLAUDE);
        std::fs::write(
            root.join("home/.claude.json"),
            r#"{"mcpServers": {"marshal": {"command": "marshal-shim"}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("home/.claude/settings.json"),
            r#"{"hooks": {
              "SessionStart": [{"hooks": [
                {"type": "command", "command": "marshal-shim claude-hook session-start"},
                {"type": "command", "command": "marshal-shim claude-hook wait", "asyncRewake": true}]}],
              "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "marshal-shim claude-hook prompt-submit"}]}]
            }}"#,
        )
        .unwrap();
        Target {
            root,
            tmux: tempfile::Builder::new().prefix("ssh-").tempdir().unwrap(),
        }
    }

    fn work(&self) -> PathBuf {
        std::fs::canonicalize(self.root.join("work")).unwrap()
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.work().join(file)).unwrap_or_default()
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("kill-server")
            .env("TMUX_TMPDIR", self.tmux.path())
            .env_remove("TMUX")
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn plan(name: &str, address: &str, cwd: &Path) -> Plan {
    Plan {
        harness: "claude".into(),
        cwd: cwd.display().to_string(),
        name: name.into(),
        launch_address: address.into(),
        daemon_address: address.into(),
        timeout: Duration::from_secs(30),
        check: true,
        options: vec!["--host".into(), "target-1".into()],
        task: Some("first task for the ssh proof".into()),
        records: None,
    }
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

#[test]
#[ignore = "needs MARSHAL_E2E_DAEMON and MARSHAL_E2E_SHIM (see the module docs)"]
fn the_ssh_adapter_registers_through_the_hook_listener_and_names_a_loopback_one() {
    // Run explicitly, a missing prerequisite fails rather than skips.
    env("MARSHAL_E2E_DAEMON").expect("set MARSHAL_E2E_DAEMON to a marshal-daemon binary");
    let shim = env("MARSHAL_E2E_SHIM")
        .expect("set MARSHAL_E2E_SHIM to a marshal-shim of this tree's version");
    let ip = lan_ip().expect("needs a non-loopback address");
    assert!(have("tmux", "-V"), "needs tmux");
    assert!(
        have("python3", "--version") || have("jq", "--version"),
        "needs python3 or jq"
    );
    let target = Target::new(Path::new(&shim));
    let adapter_dir = Path::new(env!("CARGO_BIN_EXE_marshal-launch-ssh"))
        .parent()
        .unwrap()
        .to_path_buf();
    let fake_ssh = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-ssh");
    let path = format!(
        "{}:{}:{}",
        target.root.join("bin").display(),
        adapter_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // SAFETY: this binary's only test sets these before it starts any thread
    // of its own; the adapter, ssh and the target inherit them.
    unsafe {
        std::env::set_var("PATH", &path);
        std::env::set_var("HOME", target.root.join("home"));
        std::env::set_var("TMUX_TMPDIR", target.tmux.path());
        std::env::set_var("MARSHAL_LAUNCH_SSH", &fake_ssh);
        std::env::remove_var("TMUX");
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("MARSHAL_HOST");
    }
    let ssh = ExternalBackend::find_in("ssh", std::ffi::OsStr::new(&path)).unwrap();
    assert_eq!(ssh.program(), adapter_dir.join("marshal-launch-ssh"));

    // Both listeners on the reachable address.
    let d = daemon::RealDaemon::from_env_on(ip, ip).expect("start the daemon");
    // SAFETY: as above; nothing else reads the environment concurrently.
    unsafe { std::env::set_var("MARSHAL_BASE_URL", &d.hook_url) };
    let work = target.work();
    let outcome = match block_on(marshal_launch::launch(
        &ssh,
        &plan("ssh-proof", &d.address, &work),
    )) {
        Ok(o) => o,
        Err(e) => panic!(
            "launch failed: {e:#}\nsession-start: {}\ndaemon log: {}",
            target.read("session-start.out"),
            std::fs::read_to_string(&d.log).unwrap_or_default()
        ),
    };
    let host = String::from_utf8(Command::new("uname").arg("-n").output().unwrap().stdout).unwrap();
    // The row the launcher joined is the one the stub's SessionStart hook
    // registered through the hook listener.
    assert_eq!(outcome.joined.session_id, target.read("session-id").trim());
    assert_eq!(outcome.launched.host, host.trim());
    assert_eq!(outcome.launched.cwd, work.display().to_string());
    assert_eq!(
        outcome.launched.handle,
        format!("target-1 marshal-{}", outcome.agent_id)
    );
    assert!(
        outcome.agent_id.starts_with("launch-"),
        "{}",
        outcome.agent_id
    );
    eprintln!(
        "joined {} (agent {}): the row the stub's SessionStart hook registered through {}",
        outcome.joined.session_id, outcome.agent_id, d.hook_url
    );

    // The first task reaches the target through the hook listener too.
    let started = Instant::now();
    while !target
        .read("prompt-submit.out")
        .contains("first task for the ssh proof")
    {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the first task never reached the target: {:?}",
            target.read("prompt-submit.out")
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(outcome.task_message_id.is_some());
    eprintln!(
        "first task {} reached the target through prompt-submit: {}",
        outcome.task_message_id.as_deref().unwrap_or(""),
        target.read("prompt-submit.out").trim()
    );

    assert_eq!(ssh.status(&outcome.launched.handle).unwrap(), Some(true));
    ssh.stop(&outcome.launched.handle).unwrap();
    assert_eq!(ssh.status(&outcome.launched.handle).unwrap(), Some(false));
    drop(d);

    // The hook listener on loopback: the target can't reach it, the check
    // says to look at MARSHAL_HOOK_BIND, and nothing starts.
    let d = daemon::RealDaemon::from_env_on(ip, IpAddr::V4(Ipv4Addr::LOCALHOST))
        .expect("start the daemon");
    let hook_port = d.hook_url.rsplit(':').next().unwrap().trim_end_matches('/');
    // The target is told the hook is at the reachable address, where nothing listens.
    // SAFETY: as above.
    unsafe { std::env::set_var("MARSHAL_BASE_URL", format!("http://{ip}:{hook_port}")) };
    let started = Instant::now();
    let err = block_on(marshal_launch::launch(
        &ssh,
        &plan("ssh-loopback-hook", &d.address, &work),
    ))
    .unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("the ssh adapter's check failed"), "{err}");
    assert!(err.contains("hook listener"), "{err}");
    assert!(err.contains("MARSHAL_HOOK_BIND"), "{err}");
    assert!(!err.contains("can't reach the daemon at"), "{err}");
    eprintln!("hook listener on loopback, refused: {err}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "it waited out the timeout"
    );
    let listed = Command::new("tmux")
        .arg("list-sessions")
        .env("TMUX_TMPDIR", target.tmux.path())
        .env_remove("TMUX")
        .output()
        .expect("query the private tmux server");
    assert!(
        listed.stdout.is_empty(),
        "unreachable hook listener started a session"
    );
}
