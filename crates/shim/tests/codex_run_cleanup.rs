#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    os::unix::process::ExitStatusExt,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(5);

struct RunningLauncher {
    child: Child,
    descendants: Vec<u32>,
    _temp: tempfile::TempDir,
}

impl Drop for RunningLauncher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for pid in &self.descendants {
            unsafe {
                libc::kill(*pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

#[test]
fn codex_run_normal_exit_stops_bridge() {
    let mut launcher = start_launcher(true);
    let bridge = launcher.descendants[0];

    let status = launcher.child.wait().expect("wait for launcher");
    assert!(status.success());
    assert_process_exits(bridge);
}

#[test]
fn codex_run_death_stops_bridge() {
    let mut launcher = start_launcher(false);
    let bridge = start_tui(&mut launcher);

    unsafe {
        libc::kill(launcher.child.id() as libc::pid_t, libc::SIGKILL);
    }
    let status = launcher.child.wait().expect("wait for launcher");
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert_process_exits(bridge);
}

fn start_launcher(exit_tui: bool) -> RunningLauncher {
    let temp = tempfile::tempdir().expect("temporary fake Codex directory");
    let codex = temp.path().join("codex");
    let marker = temp.path().join("tui-started");
    fs::write(
        &codex,
        "#!/bin/sh\nif [ \"$1\" = app-server ]; then exit 0; fi\ntouch \"$FAKE_CODEX_MARKER\"\nif [ \"$FAKE_CODEX_EXIT\" = 1 ]; then exit 0; fi\nexec sleep 300\n",
    )
    .expect("write fake Codex");
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755))
        .expect("make fake Codex executable");

    let child = Command::new(env!("CARGO_BIN_EXE_marshal-shim"))
        .arg("codex-run")
        .env("MARSHAL_CODEX_BIN", &codex)
        .env("FAKE_CODEX_MARKER", &marker)
        .env("FAKE_CODEX_EXIT", if exit_tui { "1" } else { "0" })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start codex-run");
    let mut launcher = RunningLauncher {
        child,
        descendants: Vec::new(),
        _temp: temp,
    };
    let bridge = wait_for_bridge(launcher.child.id());
    launcher.descendants.push(bridge);

    let args = process_args(bridge).expect("read bridge command line");
    let ready_index = args
        .iter()
        .position(|arg| arg == "--ready-file")
        .expect("bridge ready-file argument");
    fs::write(&args[ready_index + 1], "ready").expect("release launcher startup");
    wait_until(|| marker.is_file(), "fake Codex TUI did not start");
    launcher
}

fn start_tui(launcher: &mut RunningLauncher) -> u32 {
    let bridge = launcher.descendants[0];
    for pid in child_pids(launcher.child.id()) {
        if pid != bridge && !launcher.descendants.contains(&pid) {
            launcher.descendants.push(pid);
        }
    }
    assert!(
        process_exists(bridge),
        "bridge exited before the test action"
    );
    bridge
}

fn wait_for_bridge(launcher: u32) -> u32 {
    let mut bridge = None;
    wait_until(
        || {
            bridge = child_pids(launcher).into_iter().find(|pid| {
                process_args(*pid).is_some_and(|args| args.iter().any(|arg| arg == "codex-bridge"))
            });
            bridge.is_some()
        },
        "codex-run did not start a bridge",
    );
    bridge.expect("bridge pid")
}

fn child_pids(parent: u32) -> Vec<u32> {
    fs::read_to_string(format!("/proc/{parent}/task/{parent}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

fn process_args(pid: u32) -> Option<Vec<String>> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        bytes
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect(),
    )
}

fn process_exists(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(") ")
                .map(|(_, rest)| rest.starts_with('Z'))
        })
        == Some(false)
}

fn assert_process_exits(pid: u32) {
    wait_until(
        || !process_exists(pid),
        &format!("bridge {pid} survived its launcher"),
    );
}

fn wait_until(mut predicate: impl FnMut() -> bool, failure: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while !predicate() {
        assert!(Instant::now() < deadline, "{failure}");
        thread::sleep(Duration::from_millis(25));
    }
}
