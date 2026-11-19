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
    let req = LaunchRequest::from(marshal_launch::ProcessRequest {
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
    });

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
        let req = LaunchRequest::from(marshal_launch::ProcessRequest {
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
        });
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

#[tokio::test(flavor = "current_thread")]
async fn sync_backend_preserves_literal_argv_inside_an_async_caller() {
    let Some(server) = common::PrivateTmux::new("literal-argv") else {
        return;
    };
    let work = tempfile::tempdir().unwrap();
    let t = Tmux::new("tmux").with_socket(&server.socket);
    let literal = [
        "space value",
        "Unicode 雪",
        "quote'\"",
        "; touch injected",
        "$(touch injected)",
    ];
    let mut argv = [
        "sh",
        "-c",
        "printf '%s\\n' \"$@\" > literal-argv; sleep 30",
        "sh",
    ]
    .map(str::to_owned)
    .to_vec();
    argv.extend(literal.map(str::to_owned));
    let req = LaunchRequest {
        version: PROTOCOL_VERSION,
        harness: "claude".into(),
        argv,
        cwd: work.path().display().to_string(),
        name: "literal".into(),
        agent_id: "literal-argv".into(),
        marshal_version: MARSHAL_VERSION.into(),
        env: Default::default(),
        options: vec![],
    };
    let launched = t.launch(&req).unwrap();
    let output = work.path().join("literal-argv");
    for _ in 0..50 {
        if std::fs::read_to_string(&output).unwrap_or_default()
            == format!("{}\n", literal.join("\n"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(output).unwrap(),
        format!("{}\n", literal.join("\n"))
    );
    assert!(!work.path().join("injected").exists());
    assert_eq!(t.recover(&req).unwrap(), Some(launched.clone()));
    assert_eq!(t.stop(&launched.handle).unwrap(), StopReceipt::Stopped);
}

#[test]
fn unreachable_existing_endpoint_is_not_reported_as_a_stopped_agent() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("not-a-socket");
    std::fs::write(&socket, b"occupied").unwrap();
    let executable = directory.path().join("tmux-permission-denied");
    std::fs::write(&executable, "#!/bin/sh\nfor arg do [ \"$arg\" != -V ] || { echo 'tmux 3.4'; exit 0; }; done\necho 'error connecting to fixture (Permission denied)' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let t = Tmux::new(executable.display().to_string()).with_socket(socket);
    assert!(t.status("marshal-endpoint").is_err());
    assert!(t.stop("marshal-endpoint").is_err());
}

#[test]
fn existing_endpoint_is_retried_after_a_transient_connection_refusal() {
    use std::os::unix::fs::PermissionsExt;
    let Some(server) = common::PrivateTmux::new("startup-discovery") else {
        return;
    };
    let work = tempfile::tempdir().unwrap();
    let t = Tmux::new("tmux").with_socket(&server.socket);
    let req = LaunchRequest {
        version: PROTOCOL_VERSION,
        harness: "claude".into(),
        argv: ["sh", "-c", "sleep 30"].map(str::to_owned).to_vec(),
        cwd: work.path().display().to_string(),
        name: "startup".into(),
        agent_id: "startup-discovery".into(),
        marshal_version: MARSHAL_VERSION.into(),
        env: Default::default(),
        options: vec![],
    };
    let launched = t.launch(&req).unwrap();
    let wrapper = work.path().join("tmux-startup");
    std::fs::write(&wrapper, format!("#!/bin/sh\nfor arg do\nif [ \"$arg\" = list-sessions ] && mkdir '{}' 2>/dev/null; then\necho 'error connecting to fixture (Connection refused)' >&2\nexit 1\nfi\ndone\nexec tmux \"$@\"\n", work.path().join("first-discovery").display())).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let racing = Tmux::new(wrapper.display().to_string()).with_socket(&server.socket);
    assert_eq!(racing.status(&launched.handle).unwrap(), Some(true));
    assert!(work.path().join("first-discovery").exists());
    assert_eq!(t.stop(&launched.handle).unwrap(), StopReceipt::Stopped);
}

#[test]
fn concurrent_launches_converge_and_foreign_owner_is_never_stopped() {
    let Some(server) = common::PrivateTmux::new("collision") else {
        return;
    };
    let work = tempfile::tempdir().unwrap();
    let t = Tmux::new("tmux").with_socket(&server.socket);
    let req = LaunchRequest {
        version: PROTOCOL_VERSION,
        harness: "claude".into(),
        argv: ["sh", "-c", "printf 'started\\n' >> starts; sleep 30"]
            .map(str::to_owned)
            .to_vec(),
        cwd: work.path().display().to_string(),
        name: "collision".into(),
        agent_id: "collision".into(),
        marshal_version: MARSHAL_VERSION.into(),
        env: Default::default(),
        options: vec![],
    };
    let barrier = std::sync::Barrier::new(8);
    let launches = std::thread::scope(|scope| {
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    t.launch(&req).unwrap()
                })
            })
            .collect();
        tasks
            .into_iter()
            .map(|task| task.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(launches.iter().all(|launch| launch == &launches[0]));
    let starts = work.path().join("starts");
    for _ in 0..50 {
        if starts.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(starts).unwrap(), "started\n");
    let handle = &launches[0].handle;
    assert!(
        server
            .tmux()
            .args([
                "set-environment",
                "-t",
                handle,
                "MARSHAL_LAUNCH_KEY",
                "foreign-owner"
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        t.stop(handle)
            .unwrap_err()
            .to_string()
            .contains("does not identify this launch")
    );
    assert!(
        server
            .tmux()
            .args(["has-session", "-t", handle])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        server
            .tmux()
            .args([
                "set-environment",
                "-t",
                handle,
                "MARSHAL_LAUNCH_KEY",
                handle
            ])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(t.stop(handle).unwrap(), StopReceipt::Stopped);
}
