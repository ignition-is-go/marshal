//! marshal-launch end to end against an in-process Myko cell carrying
//! Marshal's entities and command handlers (the daemon's Pulse-backed parts
//! aren't needed), with the built-in tmux adapter on a private tmux server.
//!
//! The harness is a stub `claude` (tests/fixtures/stub-harness) that prints
//! the environment it was given and idles. The test registers its roster row
//! as Claude's hooks would, from what the stub printed, so this covers the
//! launcher's side: the env reaching the agent, the join proof (an impostor
//! row with the right id in the wrong folder is passed over), the first task
//! as one direct message from a short-lived `launcher` row, the launch record,
//! stop, and an unreachable daemon starting nothing. A real unattended Claude
//! is covered by first_task_e2e.rs.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use common::cell::{Cell, free_port};
use marshal_entities::GetAllMessages;
use marshal_launch::record::Store;
use marshal_launch::tmux::{Tmux, this_host};
use marshal_launch::{LaunchBackend, MARSHAL_VERSION, Plan};

const WAIT: Duration = Duration::from_secs(10);

fn run_with_isolated_home(test: &str) -> bool {
    const ISOLATED: &str = "MARSHAL_LAUNCH_E2E_ISOLATED";
    if std::env::var_os(ISOLATED).is_some() {
        return true;
    }
    let home = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(ISOLATED, "1")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env_remove("MARSHAL_BASE_URL")
        .env_remove("MARSHAL_HOST")
        .status()
        .unwrap();
    assert!(status.success(), "isolated test {test} failed");
    false
}

#[test]
fn launch_join_first_task_record_and_stop() {
    if !run_with_isolated_home("launch_join_first_task_record_and_stop") {
        return;
    }
    let Some(server) = common::PrivateTmux::new("e2e") else {
        return;
    };
    let cell = Cell::spawn();
    // The launch probe dials the hook port (6156) on the launch address's host.
    // Hold it if nothing does, so the probe passes as it would beside a daemon.
    let _hook = std::net::TcpListener::bind(("127.0.0.1", marshal_launch::address::HOOK_PORT));

    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("launch-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = std::fs::canonicalize(&cwd).unwrap();
    let records = root.join("state/marshal/launches.json");

    let stubs = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stub-harness");
    let path = format!(
        "{}:{}",
        stubs.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let tmux = Tmux::new("tmux")
        .with_socket(&server.socket)
        .with_client_env("PATH", path)
        .with_shim(common::shim_at(MARSHAL_VERSION));

    let plan = Plan {
        harness: "claude".into(),
        cwd: cwd.display().to_string(),
        name: "e2e-agent".into(),
        launch_address: cell.address.clone(),
        daemon_address: cell.address.clone(),
        timeout: Duration::from_secs(20),
        check: true,
        options: vec![],
        task: Some("say ready".into()),
        records: Some(records.clone()),
    };

    // Launch on its own thread; this one plays the agent's hooks.
    let launching = {
        let tmux = tmux.clone();
        let plan = plan.clone();
        thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(marshal_launch::launch(&tmux, &plan))
        })
    };

    // The stub shows the id and launch address it was given, in the folder asked for.
    let started = Instant::now();
    let (agent_id, given_address) = loop {
        let text = server
            .sessions()
            .first()
            .map_or_else(String::new, |name| server.pane(name));
        if let Some(line) = text.lines().find(|l| l.starts_with("[stub ")) {
            let parts: Vec<&str> = line
                .trim_start_matches("[stub ")
                .trim_end_matches(']')
                .split(' ')
                .collect();
            assert_eq!(parts.len(), 3, "{line}");
            assert_eq!(parts[2], cwd.display().to_string(), "{line}");
            break (parts[0].to_string(), parts[1].to_string());
        }
        assert!(started.elapsed() < WAIT, "the stub never started: {text:?}");
        thread::sleep(Duration::from_millis(100));
    };
    assert!(agent_id.starts_with("launch-"), "{agent_id}");
    assert_eq!(given_address, cell.address);

    // The right id in the wrong folder isn't taken for the launched agent.
    cell.register("impostor", &agent_id, &this_host(), "/elsewhere");
    thread::sleep(Duration::from_millis(600));
    assert!(
        !launching.is_finished(),
        "an impostor row was taken for the launch"
    );
    cell.register(
        "launched-agent",
        &agent_id,
        &this_host(),
        &cwd.display().to_string(),
    );

    let outcome = launching.join().unwrap().expect("launch");
    assert_eq!(outcome.agent_id, agent_id);
    assert_eq!(outcome.joined.session_id, "launched-agent");
    assert_eq!(outcome.launched.host, this_host());
    assert_eq!(
        server.sessions().as_slice(),
        std::slice::from_ref(&outcome.launched.handle)
    );
    assert_eq!(outcome.launched.cwd, cwd.display().to_string());

    // The first task: one direct message, labelled, from a launcher row that's gone now.
    let message_id = outcome.task_message_id.expect("a task was sent");
    let messages = cell.context().exec_query(GetAllMessages {}).unwrap();
    let to_agent: Vec<_> = messages
        .iter()
        .filter(|m| {
            m.to_session_id
                .as_ref()
                .is_some_and(|t| t.0.as_ref() == "launched-agent")
        })
        .collect();
    assert_eq!(to_agent.len(), 1, "{to_agent:?}");
    let m = to_agent[0];
    assert_eq!(m.id.0.as_ref(), message_id);
    assert!(
        m.body.starts_with("[first task from marshal-launch on "),
        "{}",
        m.body
    );
    assert!(
        m.body.contains(&agent_id) && m.body.ends_with("] say ready"),
        "{}",
        m.body
    );
    let sender = m.from_session_id.0.to_string();
    let started = Instant::now();
    while cell.sessions().iter().any(|s| s.id.0.as_ref() == sender) {
        assert!(
            started.elapsed() < WAIT,
            "the launcher's row stayed on the roster"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert!(!messages.iter().any(|m| {
        m.to_session_id
            .as_ref()
            .is_some_and(|t| t.0.as_ref() == "impostor")
    }));

    // Recorded, with the session it joined as; stop ends it and forgets it.
    let store = Store::at(&records);
    let recorded = store.list().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].agent_id, agent_id);
    assert_eq!(recorded[0].session_id.as_deref(), Some("launched-agent"));
    assert_eq!(tmux.status(&recorded[0].handle).unwrap(), Some(true));
    tmux.stop(&recorded[0].handle).unwrap();
    store.remove(&agent_id).unwrap();
    assert_eq!(tmux.status(&outcome.launched.handle).unwrap(), Some(false));
    assert!(store.list().unwrap().is_empty());

    // An unreachable daemon starts nothing.
    let mut unreachable = plan.clone();
    unreachable.name = "e2e-unreachable".into();
    unreachable.daemon_address = format!("ws://127.0.0.1:{}", free_port());
    unreachable.timeout = Duration::from_secs(2);
    let err = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(marshal_launch::launch(&tmux, &unreachable))
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("nothing was started"),
        "{err:#}"
    );
    assert!(server.sessions().is_empty());
    assert!(store.list().unwrap().is_empty());

    std::fs::remove_dir_all(&root).unwrap();
}

/// This machine's address on its default route, if it has one. No packet is sent.
fn lan_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("10.255.255.255:9").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// The daemon port exposed and the hook port left on loopback: the launch
/// fails naming the hook listener before anything starts, rather than timing out.
#[test]
fn an_exposed_daemon_with_a_loopback_hook_port_is_named() {
    if !run_with_isolated_home("an_exposed_daemon_with_a_loopback_hook_port_is_named") {
        return;
    }
    let Some(server) = common::PrivateTmux::new("hook") else {
        return;
    };
    let Some(ip) = lan_ip() else {
        eprintln!("skipped: no non-loopback address here");
        return;
    };
    let hook = (ip, marshal_launch::address::HOOK_PORT);
    if std::net::TcpStream::connect_timeout(&hook.into(), Duration::from_millis(500)).is_ok() {
        eprintln!("skipped: something already listens on {ip}:6156");
        return;
    }
    // MARSHAL_BIND exposed (all interfaces); nothing serves the hook port there.
    let cell = Cell::spawn_on("0.0.0.0");
    let port = cell.address.rsplit(':').next().unwrap().to_string();
    let exposed = format!("ws://{ip}:{port}");
    let tmux = Tmux::new("tmux")
        .with_socket(&server.socket)
        .with_shim(common::shim_at(MARSHAL_VERSION));
    let plan = Plan {
        harness: "claude".into(),
        cwd: env!("CARGO_MANIFEST_DIR").into(),
        name: "e2e-hook".into(),
        launch_address: exposed.clone(),
        daemon_address: exposed.clone(),
        timeout: Duration::from_secs(5),
        check: true,
        options: vec![],
        task: None,
        records: None,
    };
    let started = Instant::now();
    let err = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(marshal_launch::launch(&tmux, &plan))
        .unwrap_err();
    let err = format!("{err:#}");
    assert!(
        err.contains("hook listener")
            && err.contains("MARSHAL_HOOK_BIND")
            && err.contains("nothing was"),
        "{err}"
    );
    assert!(
        started.elapsed() < plan.timeout,
        "it waited out the timeout: {err}"
    );
    assert!(server.sessions().is_empty());
}
