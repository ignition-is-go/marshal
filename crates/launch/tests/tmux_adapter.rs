//! The built-in tmux adapter on a private tmux server: launch runs argv in the
//! canonical folder with env added, check refuses a mismatched shim and
//! status follows the session, and stop ends it.

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use marshal_launch::tmux::{Tmux, this_host};
use marshal_launch::{
    LaunchBackend, LaunchRequest, MARSHAL_VERSION, PROTOCOL_VERSION, StopReceipt,
};

#[test]
fn launch_status_and_stop_on_a_private_tmux_server() {
    let Some(server) = common::PrivateTmux::new("adapter") else {
        return;
    };
    let t = Tmux::new("tmux")
        .with_socket(&server.socket)
        .with_shim(common::shim_at(MARSHAL_VERSION));
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let req = LaunchRequest {
        version: PROTOCOL_VERSION,
        harness: "claude".into(),
        argv: ["sh", "-c", "echo \"[$MARSHAL_AGENT_ID $PWD]\"; sleep 30"]
            .map(String::from)
            .to_vec(),
        cwd: dir.display().to_string(),
        name: "launch-test".into(),
        agent_id: "launch-t".into(),
        marshal_version: MARSHAL_VERSION.into(),
        env: [("MARSHAL_AGENT_ID".to_string(), "launch-t".to_string())].into(),
        options: vec![],
    };

    let old = Tmux::new("tmux")
        .with_socket(&server.socket)
        .with_shim(common::shim_at("0.1.0"));
    let err = old.check(&req).unwrap_err().to_string();
    assert!(
        err.contains("0.1.0") && err.contains("major.minor"),
        "{err}"
    );

    t.check(&req).unwrap();
    let l = t.launch(&req).unwrap();
    let canon = std::fs::canonicalize(&dir).unwrap();
    assert_eq!(l.handle, t.recovery_handle(&req).unwrap());
    assert_eq!(l.host, this_host());
    assert_eq!(l.cwd, canon.display().to_string());
    assert_eq!(l.r#where, format!("{} tmux:launch-test", this_host()));
    assert!(
        l.attach
            .as_ref()
            .unwrap()
            .contains(&server.socket.display().to_string())
    );
    assert_eq!(t.status(&l.handle).unwrap(), Some(true));
    assert_eq!(t.recover(&req).unwrap(), Some(l.clone()));
    assert_eq!(t.launch(&req).unwrap(), l);

    let want = format!("[launch-t {}]", canon.display());
    let mut seen = String::new();
    for _ in 0..50 {
        seen = server.pane(&l.handle);
        if seen.contains(&want) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(seen.contains(&want), "{seen:?}");

    assert!(matches!(t.stop(&l.handle).unwrap(), StopReceipt::Stopped));
    assert_eq!(t.status(&l.handle).unwrap(), Some(false));
    let mut replacement = req.clone();
    replacement.agent_id = "launch-replacement".to_owned();
    let other = t.launch(&replacement).unwrap();
    assert_ne!(other.handle, l.handle);
    assert!(matches!(t.stop(&l.handle).unwrap(), StopReceipt::Gone));
    assert_eq!(t.status(&other.handle).unwrap(), Some(true));
    t.stop(&other.handle).unwrap();
}

#[test]
fn workers_strip_daemon_credentials_from_new_and_running_tmux_servers() {
    const KEYS: [&str; 3] = [
        "MYKO_POSTGRES_URL",
        "MARSHAL_AUTH_POSTGRES_URL",
        "MARSHAL_AUTH_CLIENT_SECRET",
    ];
    for existing in [false, true] {
        let label = if existing { "existing-env" } else { "new-env" };
        let Some(server) = common::PrivateTmux::new(label) else {
            return;
        };
        if existing {
            let mut command = server.tmux();
            command.envs(KEYS.map(|key| (key, "synthetic")));
            assert!(
                command
                    .args(["new-session", "-d", "-s", "sentinel", "sleep", "30"])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let t = KEYS.iter().fold(
            Tmux::new("tmux").with_socket(&server.socket),
            |tmux, key| tmux.with_client_env(*key, "synthetic"),
        );
        let work = tempfile::tempdir().unwrap();
        let req = LaunchRequest {
            version: PROTOCOL_VERSION,
            harness: "claude".into(),
            argv: [
                "sh",
                "-c",
                r#"if [ "$CLAUDE_CODE_OAUTH_TOKEN" = provisioned ] && ! env | grep -Eq '^(MYKO_POSTGRES_URL|MARSHAL_AUTH_POSTGRES_URL|MARSHAL_AUTH_CLIENT_SECRET)='; then echo worker-env-safe; fi; sleep 30"#,
            ].map(String::from).to_vec(),
            cwd: work.path().display().to_string(),
            name: label.into(),
            agent_id: format!("launch-{label}"),
            marshal_version: MARSHAL_VERSION.into(),
            env: [
                ("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "provisioned".to_string()),
                ("MYKO_POSTGRES_URL".to_string(), "synthetic-request".to_string()),
            ].into(),
            options: vec![],
        };
        let launched = t.launch(&req).unwrap();
        let mut pane = String::new();
        for _ in 0..20 {
            pane = server.pane(&launched.handle);
            if pane.contains("worker-env-safe") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            pane.contains("worker-env-safe"),
            "{label}: worker environment is incorrect"
        );
        let out = server
            .tmux()
            .args(["show-environment", "-g", KEYS[0]])
            .output()
            .unwrap();
        assert_eq!(
            out.status.success(),
            existing,
            "{label}: tmux server inherited unexpected credentials"
        );
        t.stop(&launched.handle).unwrap();
    }
}
