//! The built-in tmux adapter on a private tmux server: launch runs argv in the
//! canonical folder with env added, check refuses a mismatched shim and
//! status follows the session, and stop ends it.

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use marshal_launch::tmux::{Tmux, this_host};
use marshal_launch::{LaunchBackend, LaunchRequest, MARSHAL_VERSION, StopReceipt};

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
    let command = l.attachment_plan.as_ref().unwrap().plan().command();
    command.validate().unwrap();
    assert_eq!(command.program, "tmux");
    assert_eq!(
        command.args,
        vec![
            "-u".to_string(),
            "-S".into(),
            server.socket.display().to_string(),
            "attach-session".into(),
            "-t".into(),
            format!("={}", l.handle)
        ]
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
        "",
        ";",
        "trailing;",
        "backslash\\;",
        "line\nbreak",
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
    let req = LaunchRequest::from(marshal_launch::ProcessRequest {
        harness: "claude".into(),
        argv,
        cwd: work.path().display().to_string(),
        name: "literal".into(),
        agent_id: "literal-argv".into(),
        marshal_version: MARSHAL_VERSION.into(),
        env: Default::default(),
        options: vec![],
    });
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
    let req = LaunchRequest::from(marshal_launch::ProcessRequest {
        harness: "claude".into(),
        argv: ["sh", "-c", "sleep 30"].map(str::to_owned).to_vec(),
        cwd: work.path().display().to_string(),
        name: "startup".into(),
        agent_id: "startup-discovery".into(),
        marshal_version: MARSHAL_VERSION.into(),
        env: Default::default(),
        options: vec![],
    });
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
    let req = LaunchRequest::from(marshal_launch::ProcessRequest {
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
    });
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

fn fixture_program(directory: &std::path::Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let executable = directory.join(name);
    let mut body_path = executable.as_os_str().to_os_string();
    body_path.push(".body");
    std::fs::write(PathBuf::from(body_path), body).unwrap();
    let driver = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/executable-driver");
    let mut permissions = std::fs::metadata(&driver).unwrap().permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    std::fs::set_permissions(&driver, permissions).unwrap();
    std::os::unix::fs::symlink(driver, &executable).unwrap();
    executable
}

#[test]
fn backend_errors_redact_environment_and_worker_arguments() {
    for phase in ["refusal", "lookup-timeout", "create-timeout"] {
        let directory = tempfile::tempdir().unwrap();
        let secret = "sk-ant-oat01-synthetic-timeout-secret";
        let argument = "synthetic-private-worker-argument";
        let refusal = if phase == "create-timeout" {
            "sleep 7"
        } else {
            "exit 1"
        };
        let lookup = if phase == "lookup-timeout" {
            "sleep 7"
        } else {
            "exit 1"
        };
        let executable = fixture_program(
            directory.path(),
            "tmux-redaction",
            &format!(
                "for arg do\ncase \"$arg\" in\n-V) echo 'tmux 3.4'; exit 0;;\nlist-sessions) echo 'no server running on fixture' >&2; {lookup};;\nnew-session) echo '{secret} {argument}' >&2; {refusal};;\nesac\ndone\nexit 1\n"
            ),
        );
        let backend = Tmux::new(executable.display().to_string())
            .with_socket(directory.path().join("socket"));
        let request = LaunchRequest::from(marshal_launch::ProcessRequest {
            harness: "claude".into(),
            argv: ["sh", argument].map(str::to_owned).to_vec(),
            cwd: directory.path().display().to_string(),
            name: "redaction".into(),
            agent_id: "redaction".into(),
            marshal_version: MARSHAL_VERSION.into(),
            env: [("CLAUDE_CODE_OAUTH_TOKEN".into(), secret.into())].into(),
            options: vec![],
        });
        let error = backend.launch(&request).unwrap_err();
        let tmux_error = error.downcast_ref::<tmux_session::Error>().unwrap();
        assert_eq!(
            tmux_error.kind(),
            if phase == "refusal" {
                tmux_session::ErrorKind::CommandFailed
            } else {
                tmux_session::ErrorKind::Timeout
            }
        );
        if phase == "create-timeout" {
            assert!(tmux_error.effect_uncertain());
        }
        for diagnostic in [
            format!("{error}"),
            format!("{error:#}"),
            format!("{error:?}"),
        ] {
            assert!(!diagnostic.contains(secret), "{diagnostic}");
            assert!(!diagnostic.contains(argument), "{diagnostic}");
        }
        for source in error.chain() {
            let diagnostic = format!("{source} {source:?}");
            assert!(!diagnostic.contains(secret), "{diagnostic}");
            assert!(!diagnostic.contains(argument), "{diagnostic}");
        }
    }
}

#[test]
fn worker_with_no_arguments_runs_as_one_literal_program() {
    let Some(server) = common::PrivateTmux::new("zero-args") else {
        return;
    };
    let work = tempfile::tempdir().unwrap();
    let worker = fixture_program(
        work.path(),
        "worker with spaces;",
        "printf '%s' \"$#\" > argc\nsleep 30\n",
    );
    let backend = Tmux::new("tmux").with_socket(&server.socket);
    let request = LaunchRequest::from(marshal_launch::ProcessRequest {
        harness: "claude".into(),
        argv: vec![worker.display().to_string()],
        cwd: work.path().display().to_string(),
        name: "zero-args".into(),
        agent_id: "zero-args".into(),
        marshal_version: MARSHAL_VERSION.into(),
        env: Default::default(),
        options: vec![],
    });
    let launched = backend.launch(&request).unwrap();
    let output = work.path().join("argc");
    for _ in 0..50 {
        if output.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(output).unwrap(), "0");
    assert_eq!(
        backend.stop(&launched.handle).unwrap(),
        StopReceipt::Stopped
    );
}

fn quiet_request(work: &std::path::Path, id: &str) -> LaunchRequest {
    LaunchRequest::from(marshal_launch::ProcessRequest {
        harness: "claude".into(),
        argv: ["sh", "-c", "printf 'started\\n' >> starts; exec sleep 30"]
            .map(str::to_owned)
            .to_vec(),
        cwd: work.display().to_string(),
        name: id.into(),
        agent_id: id.into(),
        marshal_version: MARSHAL_VERSION.into(),
        env: Default::default(),
        options: vec![],
    })
}

fn tmux_value(server: &common::PrivateTmux, args: &[&str]) -> String {
    let output = server.tmux().args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn quiet_setup_is_session_scoped_and_status_reads_do_not_change_it() {
    let Some(server) = common::PrivateTmux::new("quiet") else {
        return;
    };
    let unrelated = tmux_value(
        &server,
        &[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_id}",
            "-s",
            "unrelated",
            "sleep",
            "30",
        ],
    );
    tmux_value(&server, &["set-option", "-g", "status", "on"]);
    tmux_value(&server, &["set-option", "-t", &unrelated, "status", "on"]);
    let unrelated_pid = tmux_value(
        &server,
        &["display-message", "-p", "-t", &unrelated, "#{pane_pid}"],
    );
    let work = tempfile::tempdir().unwrap();
    let request = quiet_request(work.path(), "quiet-policy");
    let backend = Tmux::new("tmux").with_socket(&server.socket);
    let launched = backend.launch(&request).unwrap();
    assert_eq!(
        tmux_value(
            &server,
            &["show-options", "-A", "-v", "-t", &launched.handle, "status"]
        ),
        "off"
    );
    assert_eq!(
        tmux_value(&server, &["show-options", "-g", "-v", "status"]),
        "on"
    );
    assert_eq!(
        tmux_value(
            &server,
            &["show-options", "-A", "-v", "-t", &unrelated, "status"]
        ),
        "on"
    );
    let worker_pid = tmux_value(
        &server,
        &[
            "display-message",
            "-p",
            "-t",
            &launched.handle,
            "#{pane_pid}",
        ],
    );
    tmux_value(
        &server,
        &["set-option", "-t", &launched.handle, "status", "on"],
    );
    assert_eq!(backend.status(&launched.handle).unwrap(), Some(true));
    assert_eq!(
        tmux_value(
            &server,
            &["show-options", "-A", "-v", "-t", &launched.handle, "status"]
        ),
        "on"
    );
    assert_eq!(backend.recover(&request).unwrap(), Some(launched.clone()));
    assert_eq!(
        tmux_value(
            &server,
            &["show-options", "-A", "-v", "-t", &launched.handle, "status"]
        ),
        "off"
    );
    assert_eq!(
        tmux_value(
            &server,
            &[
                "display-message",
                "-p",
                "-t",
                &launched.handle,
                "#{pane_pid}"
            ]
        ),
        worker_pid
    );
    tmux_value(
        &server,
        &[
            "set-environment",
            "-t",
            &launched.handle,
            "MARSHAL_LAUNCH_KEY",
            "foreign",
        ],
    );
    tmux_value(
        &server,
        &["set-option", "-t", &launched.handle, "status", "on"],
    );
    assert!(backend.recover(&request).is_err());
    assert_eq!(
        tmux_value(
            &server,
            &["show-options", "-A", "-v", "-t", &launched.handle, "status"]
        ),
        "on"
    );
    assert_eq!(
        tmux_value(
            &server,
            &["display-message", "-p", "-t", &unrelated, "#{pane_pid}"]
        ),
        unrelated_pid
    );
    assert_eq!(
        tmux_value(&server, &["show-options", "-g", "-v", "status"]),
        "on"
    );
}

#[test]
fn failed_quiet_setup_recovers_the_same_worker_without_another_create() {
    let Some(server) = common::PrivateTmux::new("quiet-repair") else {
        return;
    };
    let unrelated = tmux_value(
        &server,
        &[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_id}",
            "-s",
            "unrelated",
            "sleep",
            "30",
        ],
    );
    tmux_value(&server, &["set-option", "-g", "status", "on"]);
    tmux_value(&server, &["set-option", "-t", &unrelated, "status", "on"]);
    let unrelated_pid = tmux_value(
        &server,
        &["display-message", "-p", "-t", &unrelated, "#{pane_pid}"],
    );
    let work = tempfile::tempdir().unwrap();
    let failure = work.path().join("failed-configuration");
    let wrapper = fixture_program(
        work.path(),
        "tmux-quiet-failure",
        &format!(
            "for arg do\nif [ \"$arg\" = set-option ] && mkdir '{}' 2>/dev/null; then\necho 'owned configuration failure' >&2\nexit 1\nfi\ndone\nexec tmux \"$@\"\n",
            failure.display()
        ),
    );
    let backend = Tmux::new(wrapper.display().to_string()).with_socket(&server.socket);
    let request = quiet_request(work.path(), "quiet-repair");
    let handle = backend.recovery_handle(&request).unwrap();
    let error = backend.launch(&request).unwrap_err();
    assert!(format!("{error:#}").contains(&handle), "{error:#}");
    assert!(format!("{error:#}").contains("uncertain"), "{error:#}");
    assert!(
        error
            .downcast_ref::<tmux_session::Error>()
            .unwrap()
            .effect_uncertain()
    );
    assert!(failure.is_dir());
    let worker_pid = tmux_value(
        &server,
        &["display-message", "-p", "-t", &handle, "#{pane_pid}"],
    );
    let recovered = backend
        .recover(&request)
        .unwrap()
        .expect("owned worker remains recoverable");
    assert_eq!(recovered.handle, handle);
    assert_eq!(
        tmux_value(
            &server,
            &["display-message", "-p", "-t", &handle, "#{pane_pid}"]
        ),
        worker_pid
    );
    assert_eq!(
        tmux_value(
            &server,
            &["show-options", "-A", "-v", "-t", &handle, "status"]
        ),
        "off"
    );
    assert_eq!(backend.launch(&request).unwrap(), recovered);
    let starts = work.path().join("starts");
    for _ in 0..50 {
        if starts.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(starts).unwrap(), "started\n");
    assert_eq!(
        tmux_value(&server, &["show-options", "-g", "-v", "status"]),
        "on"
    );
    assert_eq!(
        tmux_value(
            &server,
            &["show-options", "-A", "-v", "-t", &unrelated, "status"]
        ),
        "on"
    );
    assert_eq!(
        tmux_value(
            &server,
            &["display-message", "-p", "-t", &unrelated, "#{pane_pid}"]
        ),
        unrelated_pid
    );
    backend.stop(&handle).unwrap();
}
