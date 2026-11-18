//! marshal-launch against a real `marshal-daemon` (`MARSHAL_E2E_DAEMON`, for
//! example a build from this checkout), with a stand-in harness that makes
//! Claude's hook calls through a real `marshal-shim` (`MARSHAL_E2E_SHIM`, of
//! this tree's version).
//!
//! Unlike launch_e2e.rs, the test registers nothing itself. The shim's
//! `claude-hook session-start` puts the row on the roster through the daemon's
//! real hook listener, `prompt-submit` reads the first task, and
//! `session-end` clears the row when `stop` hangs the harness up.
//!
//! Ignored by default. Run with both variables set:
//!   MARSHAL_E2E_DAEMON=<marshal-daemon> MARSHAL_E2E_SHIM=<marshal-shim> \
//!     cargo test -p marshal-launch --test real_daemon_e2e -- --ignored
//! The daemon runs on free test ports with an in-memory store, never 6155/6156.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hyphae::Gettable;
use marshal_entities::{GetAllMessageReads, GetAllMessages, GetAllSessions};
use marshal_launch::tmux::Tmux;
use marshal_launch::{LaunchBackend, Plan};
use myko::client::{ConnectionStatus, MykoClient};

use common::daemon::RealDaemon;

const WAIT: Duration = Duration::from_secs(30);

fn wait_for(what: &str, daemon: &RealDaemon, mut ok: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ok() {
        assert!(
            started.elapsed() < WAIT,
            "{what} within {WAIT:?}. Daemon log:\n{}",
            daemon.log_text()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
#[ignore = "needs MARSHAL_E2E_DAEMON and MARSHAL_E2E_SHIM"]
fn a_real_daemon_sees_the_launch_join_read_its_task_and_leave() {
    let Some(shim) = std::env::var_os("MARSHAL_E2E_SHIM").map(PathBuf::from) else {
        eprintln!("skipped: MARSHAL_E2E_SHIM isn't set");
        return;
    };
    let Some(daemon) = RealDaemon::from_env() else {
        eprintln!("skipped: {} isn't set", common::daemon::DAEMON_ENV);
        return;
    };
    let Some(server) = common::PrivateTmux::new("real") else {
        return;
    };

    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("real-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cwd = root.join("work");
    let config = root.join("config");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(config.join("marshal")).unwrap();
    std::fs::write(config.join("marshal/daemon-address"), &daemon.address).unwrap();
    let cwd = std::fs::canonicalize(&cwd).unwrap();

    // The launcher passes these to a local agent (its environment): the hook
    // listener on its test port, and a config dir pointing at this daemon. This test binary
    // runs only this test, and sets them before starting any thread of its own.
    unsafe {
        std::env::set_var("MARSHAL_BASE_URL", &daemon.hook_url);
        std::env::set_var("XDG_CONFIG_HOME", &config);
        std::env::remove_var("MARSHAL_HOST");
    }

    let stubs = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hook-harness");
    let path = format!(
        "{}:{}",
        stubs.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let tmux = Tmux::new("tmux")
        .with_socket(&server.socket)
        .with_client_env("PATH", path)
        .with_client_env("MARSHAL_E2E_SHIM", shim.display().to_string())
        // The real version check, against the real shim.
        .with_shim(vec![shim.display().to_string()]);

    let plan = Plan {
        harness: "claude".into(),
        cwd: cwd.display().to_string(),
        name: "real-agent".into(),
        launch_address: daemon.address.clone(),
        daemon_address: daemon.address.clone(),
        timeout: WAIT,
        check: true,
        options: vec![],
        task: Some("say ready".into()),
        records: Some(root.join("state/marshal/launches.json")),
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let outcome = rt
        .block_on(marshal_launch::launch(&tmux, &plan))
        .unwrap_or_else(|e| panic!("launch: {e:#}\nDaemon log:\n{}", daemon.log_text()));

    // The joined row is the hook-registered one, carrying this launch's id.
    let pane = server.pane(&outcome.launched.handle);
    assert!(
        pane.contains(&format!(" {} {}]", outcome.agent_id, cwd.display())),
        "{pane}"
    );
    let session_id = outcome.joined.session_id.clone();
    assert!(
        pane.contains(&format!("[hook-harness {session_id} ")),
        "{pane}"
    );

    // An observer of our own, to read the daemon's state.
    marshal_entities::link();
    let client = MykoClient::new();
    let sessions = client.watch_query::<GetAllSessions>(GetAllSessions {});
    let messages = client.watch_query::<GetAllMessages>(GetAllMessages {});
    let reads = client.watch_query::<GetAllMessageReads>(GetAllMessageReads {});
    client.set_address(Some(daemon.address.clone()));
    wait_for("the observer didn't connect", &daemon, || {
        matches!(
            client.connection_status_sync(),
            ConnectionStatus::Connected(_)
        )
    });

    // The first task: one direct message, read through the hook bridge.
    let message_id = outcome.task_message_id.clone().expect("a task was sent");
    wait_for("the first task wasn't stored", &daemon, || {
        messages.get().iter().any(|m| m.id.0.as_ref() == message_id)
    });
    let m: Arc<_> = messages
        .get()
        .iter()
        .find(|m| m.id.0.as_ref() == message_id)
        .cloned()
        .unwrap();
    assert_eq!(
        m.to_session_id.as_ref().map(|t| t.0.as_ref()),
        Some(session_id.as_str())
    );
    assert!(m.body.ends_with("] say ready"), "{}", m.body);
    wait_for("the agent didn't read its first task", &daemon, || {
        reads
            .get()
            .iter()
            .any(|r| r.message_id.0.as_ref() == message_id && r.session_id.0.as_ref() == session_id)
    });
    wait_for("the task didn't reach the agent's turn", &daemon, || {
        server.pane(&outcome.launched.handle).contains("say ready")
    });
    // The launcher's own row is gone once it has sent.
    let sender = m.from_session_id.0.to_string();
    wait_for("the launcher's row stayed on the roster", &daemon, || {
        !sessions.get().iter().any(|s| s.id.0.as_ref() == sender)
    });

    // Stop hangs the harness up; its session-end hook clears the row.
    assert!(sessions.get().iter().any(|s| s.id.0.as_ref() == session_id));
    tmux.stop(&outcome.launched.handle).unwrap();
    wait_for("the agent's row wasn't cleared after stop", &daemon, || {
        !sessions.get().iter().any(|s| s.id.0.as_ref() == session_id)
    });
    assert_eq!(tmux.status(&outcome.launched.handle).unwrap(), Some(false));

    drop(client);
    let _ = std::fs::remove_dir_all(&root);
}
