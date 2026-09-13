#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::UnixListener;
use tokio_tungstenite::{accept_async, tungstenite::Message as WebSocketMessage};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

struct RunningLauncher {
    child: Child,
    descendants: Vec<u32>,
    _app_server: FakeAppServer,
    _temp: tempfile::TempDir,
}

struct FakeAppServer {
    socket: PathBuf,
    bridge: mpsc::Receiver<u32>,
    release: Option<mpsc::Sender<()>>,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeAppServer {
    fn spawn(socket: &Path) -> Self {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (bridge_tx, bridge_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let socket = socket.to_path_buf();
        let socket_for_thread = socket.clone();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build fake app-server runtime");
            runtime.block_on(async move {
                let listener =
                    UnixListener::bind(&socket_for_thread).expect("bind fake app-server");
                ready_tx.send(()).expect("publish fake app-server endpoint");
                let (stream, _) = listener.accept().await.expect("accept bridge connection");
                bridge_tx
                    .send(
                        stream
                            .peer_cred()
                            .expect("bridge peer credentials")
                            .pid()
                            .expect("bridge peer pid") as u32,
                    )
                    .expect("publish bridge pid");
                tokio::task::spawn_blocking(move || release_rx.recv())
                    .await
                    .expect("join bridge release wait")
                    .expect("release bridge handshake");
                let mut websocket = accept_async(stream).await.expect("accept bridge WebSocket");
                let initialize = websocket
                    .next()
                    .await
                    .expect("bridge initialize frame")
                    .expect("read bridge initialize frame");
                let initialize: Value =
                    serde_json::from_str(initialize.to_text().expect("initialize text"))
                        .expect("parse initialize request");
                assert_eq!(initialize["method"], "initialize");
                websocket
                    .send(WebSocketMessage::Text(
                        json!({
                            "id": initialize.get("id").cloned().unwrap_or(json!(1)),
                            "result": {
                                "userAgent": "marshal-cleanup-test",
                                "platformFamily": "test",
                                "platformOs": "linux",
                                "codexHome": "/tmp"
                            }
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .expect("send initialize response");
                let initialized = websocket
                    .next()
                    .await
                    .expect("bridge initialized frame")
                    .expect("read bridge initialized frame");
                let initialized: Value =
                    serde_json::from_str(initialized.to_text().expect("initialized text"))
                        .expect("parse initialized notification");
                assert_eq!(initialized["method"], "initialized");
                let _ = tokio::task::spawn_blocking(move || shutdown_rx.recv()).await;
            });
        });
        ready_rx
            .recv_timeout(STARTUP_TIMEOUT)
            .expect("fake app-server endpoint");
        Self {
            socket,
            bridge: bridge_rx,
            release: Some(release_tx),
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn bridge_pid(&self) -> u32 {
        self.bridge
            .recv_timeout(STARTUP_TIMEOUT)
            .expect("bridge connection")
    }

    fn release(&mut self) {
        self.release
            .take()
            .expect("bridge release sender")
            .send(())
            .expect("release bridge handshake");
    }
}

impl Drop for FakeAppServer {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
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
    let app_server = FakeAppServer::spawn(&temp.path().join("app-server.sock"));
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
        .env("MARSHAL_CODEX_APP_SERVER_SOCKET", &app_server.socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start codex-run");
    let mut launcher = RunningLauncher {
        child,
        descendants: Vec::new(),
        _app_server: app_server,
        _temp: temp,
    };
    let bridge = launcher._app_server.bridge_pid();
    launcher.descendants.push(bridge);

    let args = process_args(bridge).expect("read bridge command line");
    let ready_index = args
        .iter()
        .position(|arg| arg == "--ready-file")
        .expect("bridge ready-file argument");
    launcher._app_server.release();
    fs::write(&args[ready_index + 1], "ready").expect("release launcher startup");
    wait_until(
        STARTUP_TIMEOUT,
        || marker.is_file(),
        "fake Codex TUI did not start",
    );
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

fn child_pids(parent: u32) -> Vec<u32> {
    let mut children: Vec<u32> =
        fs::read_to_string(format!("/proc/{parent}/task/{parent}/children"))
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|pid| pid.parse().ok())
            .collect();
    children.extend(
        fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
            .filter(|pid| process_parent(*pid) == Some(parent)),
    );
    children.sort_unstable();
    children.dedup();
    children
}

fn process_parent(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
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
        CLEANUP_TIMEOUT,
        || !process_exists(pid),
        &format!("bridge {pid} survived its launcher"),
    );
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool, failure: &str) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "{failure}");
        thread::sleep(Duration::from_millis(25));
    }
}
