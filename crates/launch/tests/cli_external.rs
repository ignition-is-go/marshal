//! `marshal-launch fake claude ...` end to end: the binary finds
//! `marshal-launch-fake` on PATH, runs check then launch through it, waits for
//! the join against an in-process Myko cell, sends the first task, records the
//! launch, and `list` and `stop` go through the same adapter.
//!
//! The fake adapter starts nothing. The test plays the agent's hooks: it reads
//! the launch's agent id from the fake's request log, then registers the row the
//! fake's response describes (host `fake-host`, cwd `/work/app`).

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use marshal_entities::GetAllMessages;

use common::cell::Cell;

const BIN: &str = env!("CARGO_BIN_EXE_marshal-launch");
const WAIT: Duration = Duration::from_secs(20);

/// This machine's address on its default route, if it has one. No packet is sent.
fn lan_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("10.255.255.255:9").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// The binary, with HOME, XDG and PATH set so it reads and writes nothing of
/// the user's and finds the fake adapter. Its local daemon address names a
/// port where nothing listens: a launch must reach the cell through its
/// `--daemon`, never through the local address.
fn launcher(root: &Path, local: &str) -> Command {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = format!(
        "{}:{}",
        fixtures.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut c = Command::new(BIN);
    c.env_clear()
        .env("PATH", path)
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("MARSHAL_DAEMON_ADDRESS", local)
        .stdin(Stdio::null());
    c
}

fn text(o: &Output) -> String {
    format!(
        "status {}\nstdout:\n{}\nstderr:\n{}",
        o.status,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn marshal_launch_fake_goes_end_to_end_through_the_fake_adapter() {
    // The cell listens on all interfaces and is reached at the LAN address, a
    // launch address a non-local adapter may use.
    let Some(ip) = lan_ip() else {
        eprintln!("skipped: no non-loopback address here");
        return;
    };
    let cell = Cell::spawn_on("0.0.0.0");
    let daemon = format!("ws://{ip}:{}", cell.address.rsplit(':').next().unwrap());
    let local = format!("ws://127.0.0.1:{}", common::cell::free_port());
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-external-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home")).unwrap();
    let log = root.join("fake.log");

    // A non-local adapter needs --cwd, and the launch address may not be loopback.
    let no_cwd = launcher(&root, &local)
        .args(["fake", "claude", "--daemon", "ws://marshal.invalid:6155"])
        .output()
        .unwrap();
    assert!(!no_cwd.status.success(), "{}", text(&no_cwd));
    assert!(
        String::from_utf8_lossy(&no_cwd.stderr).contains("--cwd is needed"),
        "{}",
        text(&no_cwd)
    );
    let loopback = launcher(&root, &local)
        .args(["fake", "claude", "--cwd", "/work/app"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&loopback.stderr).contains("loopback"),
        "{}",
        text(&loopback)
    );
    assert!(!log.exists(), "the fake was run for a refused launch");

    // A loopback hook base is refused for a non-local adapter, before it runs.
    let loop_hook = launcher(&root, &local)
        .env("MARSHAL_BASE_URL", "http://127.0.0.1:26156")
        .args([
            "fake",
            "claude",
            "--cwd",
            "/work/app",
            "--daemon",
            "ws://marshal.invalid:6155",
            "--",
        ])
        .arg(format!("--log={}", log.display()))
        .output()
        .unwrap();
    assert!(!loop_hook.status.success(), "{}", text(&loop_hook));
    assert!(
        String::from_utf8_lossy(&loop_hook.stderr)
            .contains("MARSHAL_BASE_URL http://127.0.0.1:26156 is loopback"),
        "{}",
        text(&loop_hook)
    );
    assert!(!log.exists(), "the fake was run for a loopback hook base");

    // A missing adapter is named.
    let missing = launcher(&root, &local)
        .args([
            "nope",
            "claude",
            "--cwd",
            "/w",
            "--daemon",
            "ws://marshal.invalid:6155",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("marshal-launch-nope"),
        "{}",
        text(&missing)
    );

    // The real launch. --daemon names the cell, and the launcher must watch the
    // roster there too, not at its local MARSHAL_DAEMON_ADDRESS (nothing listens).
    let mut child = launcher(&root, &local)
        .env("MARSHAL_BASE_URL", "http://marshal.invalid:26156")
        .args([
            "fake",
            "claude",
            "--cwd",
            "/work/app",
            "--name",
            "app",
            "--task",
            "say ready",
            "--daemon",
            &daemon,
            "--timeout",
            "20",
            "--",
        ])
        .arg(format!("--log={}", log.display()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // check then launch, each with the request; the agent id comes from it.
    let started = Instant::now();
    let launch_req = loop {
        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        if let Some(line) = logged.lines().find(|l| l.starts_with("launch ")) {
            assert!(logged.starts_with("check "), "check runs first:\n{logged}");
            break serde_json::from_str::<serde_json::Value>(&line["launch ".len()..]).unwrap();
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "marshal-launch exited ({status}) before launching:\n{}",
                text(&child.wait_with_output().unwrap())
            );
        }
        assert!(
            started.elapsed() < WAIT,
            "the fake was never asked to launch"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let agent_id = launch_req["agent_id"].as_str().unwrap().to_string();
    assert!(agent_id.starts_with("launch-"), "{launch_req}");
    assert_eq!(launch_req["env"]["MARSHAL_DAEMON_ADDRESS"], daemon.as_str());
    assert_eq!(launch_req["env"]["MARSHAL_AGENT_ID"], agent_id.as_str());
    assert_eq!(launch_req["argv"], serde_json::json!(["claude"]));
    assert_eq!(launch_req["cwd"], "/work/app");
    // Only local settings stay home: XDG_CONFIG_HOME isn't sent to another machine.
    assert!(
        launch_req["env"].get("XDG_CONFIG_HOME").is_none(),
        "{launch_req}"
    );
    // A non-default hook port travels to a non-local adapter too.
    assert_eq!(
        launch_req["env"]["MARSHAL_BASE_URL"],
        "http://marshal.invalid:26156"
    );

    // The agent's hooks register it where the fake said it runs.
    cell.register("fake-agent", &agent_id, "fake-host", "/work/app");
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("(fake-agent) joined from fake-host fake:app"),
        "{}",
        text(&out)
    );
    assert!(
        stdout.contains("first task sent as message"),
        "{}",
        text(&out)
    );
    // The adapter's stderr reached the user.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("fake adapter: launch ok"),
        "{}",
        text(&out)
    );

    let messages = cell.context().exec_query(GetAllMessages {}).unwrap();
    assert!(
        messages.iter().any(|m| m
            .to_session_id
            .as_ref()
            .is_some_and(|t| t.0.as_ref() == "fake-agent")
            && m.body.ends_with("] say ready")),
        "{messages:?}"
    );

    // The record keeps the daemon the launch was made against.
    let record = std::fs::read_to_string(root.join("state/marshal/launches.json")).unwrap();
    assert!(
        record.contains(&format!("\"daemon\": \"{daemon}\"")),
        "{record}"
    );

    // list asks the fake for status (handle fake-ok: running), and the recorded
    // daemon (the cell, not the local address) for the row.
    let list = launcher(&root, &local).arg("list").output().unwrap();
    assert!(list.status.success(), "{}", text(&list));
    let listed = String::from_utf8_lossy(&list.stdout);
    assert!(
        listed.contains(&agent_id) && listed.contains("fake claude"),
        "{listed}"
    );
    assert!(
        listed.contains("running") && listed.contains("on the roster as fake-agent"),
        "{listed}"
    );

    // --daemon overrides the recorded daemon: there's nothing at the local address.
    let elsewhere = launcher(&root, &local)
        .args(["list", "--daemon", &local])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&elsewhere.stdout).contains("roster unknown"),
        "{}",
        text(&elsewhere)
    );

    // An adapter with no status verb (exit 2) is "unknown", not an error; any
    // other failure (here exit 1) is shown with the adapter's reason. Recorded by hand, as two older launches.
    let store = marshal_launch::record::Store::at(root.join("state/marshal/launches.json"));
    for (id, handle) in [
        ("launch-nostatus", "fake-no-status"),
        ("launch-broken", "fake-fail=unreachable"),
    ] {
        store
            .add(marshal_launch::record::Launch {
                adapter: "fake".into(),
                handle: handle.into(),
                agent_id: id.into(),
                name: id.into(),
                harness: "claude".into(),
                host: "fake-host".into(),
                cwd: "/work/app".into(),
                r#where: "fake-host fake:old".into(),
                attach: None,
                started_at: 1,
                daemon: None,
                session_id: None,
            })
            .unwrap();
    }
    let list = launcher(&root, &local).arg("list").output().unwrap();
    assert!(list.status.success(), "{}", text(&list));
    let listed = String::from_utf8_lossy(&list.stdout).into_owned();
    let line = |id: &str| {
        listed
            .lines()
            .find(|l| l.contains(id))
            .unwrap_or_default()
            .to_string()
    };
    assert!(line("launch-nostatus").contains("\tunknown\t"), "{listed}");
    assert!(
        line("launch-broken").contains("unknown (")
            && line("launch-broken").contains("No route to host"),
        "{listed}"
    );
    store.remove("launch-nostatus").unwrap();
    store.remove("launch-broken").unwrap();

    // stop goes through the fake and forgets the launch.
    let stop = launcher(&root, &local)
        .args(["stop", "app"])
        .output()
        .unwrap();
    assert!(stop.status.success(), "{}", text(&stop));
    assert!(
        String::from_utf8_lossy(&stop.stderr).contains("fake adapter: stop ok"),
        "{}",
        text(&stop)
    );
    let list = launcher(&root, &local).arg("list").output().unwrap();
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("no launches recorded"),
        "{}",
        text(&list)
    );

    std::fs::remove_dir_all(&root).unwrap();
}
