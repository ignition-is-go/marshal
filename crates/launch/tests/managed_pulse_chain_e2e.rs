//! Opt-in managed chain through the real Pulse adapter and candidate shim/daemon.
//! The HTTP provider is an owned fixture with a configured hook-worker preset.
//! It does not exercise Kubernetes, real Pulse persistence, or model execution.
//! Run in an isolated network namespace with 192.0.2.1/32 assigned to lo.
//! Set PULSE_MARSHAL_ADAPTER, MARSHAL_E2E_DAEMON, and MARSHAL_E2E_SHIM
//! to absolute candidate binary paths. Python 3 is required.

#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::daemon::RealDaemon;
use hyphae::Gettable;
use marshal_entities::{GetAllMessageReads, GetAllMessages, GetAllSessions};
use marshal_launch::api::{ProcessState, Service, StartLaunch, TaskState};
use marshal_launch::{LaunchIntent, ManagedIntent, PulseWorkspace};
use serde_json::{Value, json};

fn wait(what: &str, mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(start.elapsed() < Duration::from_secs(30), "{what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Provider(Child);

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status();
        let _ = self.0.wait();
    }
}

fn binary(key: &str) -> PathBuf {
    std::fs::canonicalize(std::env::var_os(key).unwrap_or_else(|| panic!("set {key}")))
        .unwrap_or_else(|error| panic!("{key}: {error}"))
}

#[test]
#[ignore = "requires actual candidate Pulse adapter, daemon, shim; isolated Linux network namespace"]
fn managed_pulse_chain_joins_delivers_once_recovers_and_stops() {
    let adapter = binary("PULSE_MARSHAL_ADAPTER");
    let shim = binary("MARSHAL_E2E_SHIM");
    binary("MARSHAL_E2E_DAEMON");
    let ip = "192.0.2.1".parse().unwrap();
    let daemon = RealDaemon::from_env_on(ip, ip).expect("candidate daemon");
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    let home = root.path().join("home");
    let bin = root.path().join("bin");
    for directory in [&cwd, &home, &bin] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::os::unix::fs::symlink(&adapter, bin.join("marshal-launch-pulse")).unwrap();
    let launch_id = format!("launch-{}", uuid::Uuid::new_v4().simple());
    let ready = root.path().join("provider-url");
    let worker_log = root.path().join("worker.log");
    let config = root.path().join("provider.json");
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "launch_id": launch_id, "daemon": daemon.address, "hook": daemon.hook_url,
            "shim": shim, "cwd": cwd, "home": home, "host": marshal_launch::tmux::this_host(),
            "worker": fixtures.join("hook-harness/claude"), "ready": ready, "log": worker_log,
        }))
        .unwrap(),
    )
    .unwrap();
    let _provider = Provider(
        Command::new("python3")
            .arg(fixtures.join("pulse-provider.py"))
            .arg(&config)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait("provider ready", || ready.is_file());
    let url = std::fs::read_to_string(&ready).unwrap();
    unsafe {
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
        std::env::set_var("XDG_STATE_HOME", home.join("state"));
        std::env::set_var("PULSE_AGENTS_URL", &url);
        std::env::set_var("PULSE_AGENTS_ACCESS_TOKEN", "owned-fixture-token");
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            std::env::remove_var(key);
        }
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let service_dir = root.path().join("launches");
    let service = || {
        Service::new(
            service_dir.clone(),
            daemon.address.clone(),
            daemon.address.clone(),
            Some(daemon.hook_url.clone()),
        )
    };
    let request = StartLaunch {
        launch_id: launch_id.clone(),
        backend: "pulse".into(),
        name: "managed-chain".into(),
        check: true,
        timeout_secs: 30,
        first_task: Some("owned first task token".into()),
        intent: LaunchIntent::Managed(ManagedIntent::Pulse {
            environment_id: "fixture".into(),
            preset_id: "hook-worker".into(),
            workspace: PulseWorkspace::Session {},
        }),
    };
    let uncertain = runtime.block_on(service().start(request.clone())).unwrap();
    assert!(uncertain.error.is_some(), "fault must reach the Service");
    assert_eq!(uncertain.process, ProcessState::Unknown);
    assert_eq!(uncertain.session_id, None);
    assert_eq!(uncertain.launched, None);
    assert_eq!(uncertain.task, TaskState::Pending);
    let proof = || {
        runtime.block_on(async {
            reqwest::get(format!("{url}/proof"))
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        })
    };
    let admitted = proof();
    assert_eq!(admitted["starts"], 1);
    assert_eq!(admitted["receipt"]["turnSubmitted"], false);
    let first = runtime.block_on(service().start(request.clone())).unwrap();
    assert_eq!(
        first.error,
        None,
        "daemon={} worker={}",
        daemon.log_text(),
        std::fs::read_to_string(&worker_log).unwrap_or_default()
    );
    assert_eq!(first.process, ProcessState::Running);
    let session_id = first.session_id.clone().expect("joined worker");
    let TaskState::Stored { message_id } = first.task.clone() else {
        panic!("{:?}", first.task)
    };
    marshal_entities::link();
    let observer = myko::client::MykoClient::new();
    let sessions = observer.watch_query::<GetAllSessions>(GetAllSessions {});
    let messages = observer.watch_query::<GetAllMessages>(GetAllMessages {});
    let reads = observer.watch_query::<GetAllMessageReads>(GetAllMessageReads {});
    observer.set_address(Some(daemon.address.clone()));
    wait("first task acknowledgment through actual shim", || {
        reads.get().iter().any(|read| {
            read.message_id.0.as_ref() == message_id && read.session_id.0.as_ref() == session_id
        })
    });
    assert_eq!(
        sessions
            .get()
            .iter()
            .filter(|row| row.agent_id.as_deref() == Some(&launch_id))
            .count(),
        1
    );
    let matching = messages.get();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].id.0.as_ref(), message_id);
    assert_eq!(
        matching[0].to_session_id.as_ref().unwrap().0.as_ref(),
        session_id
    );
    assert!(matching[0].body.ends_with("] owned first task token"));
    assert_eq!(
        reads
            .get()
            .iter()
            .filter(|read| read.message_id.0.as_ref() == message_id)
            .count(),
        1
    );
    assert!(
        std::fs::read_to_string(&worker_log)
            .unwrap()
            .contains("owned first task token")
    );
    let record: Value = serde_json::from_slice(
        &std::fs::read(service_dir.join(format!("{launch_id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(
        record["backend_request"]["context"],
        json!({"daemon_address":daemon.address,"hook_base":daemon.hook_url})
    );
    let before = proof();
    assert_eq!(before["receipt"], admitted["receipt"]);
    assert_eq!(before["starts"], 1);
    assert_eq!(before["receipt"]["turnSubmitted"], false);
    let recovered = runtime.block_on(service().start(request.clone())).unwrap();
    assert_eq!(recovered.error, None);
    assert_eq!(recovered.launched, first.launched);
    assert_eq!(recovered.session_id, first.session_id);
    assert_eq!(recovered.task, first.task);
    let after = proof();
    assert_eq!(after["receipt"], before["receipt"]);
    assert_eq!(after["starts"], 1);
    assert_eq!(after["posts"].as_array().unwrap().len(), 1);
    assert_eq!(messages.get().len(), 1);
    assert_eq!(
        reads
            .get()
            .iter()
            .filter(|read| read.message_id.0.as_ref() == message_id)
            .count(),
        1
    );
    let stopped = runtime.block_on(service().stop(&launch_id)).unwrap();
    assert_eq!(stopped.error, None);
    assert_eq!(stopped.process, ProcessState::Stopped);
    wait("worker roster cleanup", || {
        !sessions
            .get()
            .iter()
            .any(|row| row.id.0.as_ref() == session_id)
    });
    assert_eq!(proof()["worker_live"], false);
    assert_eq!(proof()["worker_group_live"], false);
    assert_eq!(
        std::fs::read_to_string(&worker_log)
            .unwrap()
            .matches("owned first task token")
            .count(),
        1
    );
    assert_eq!(
        service().get(&launch_id).unwrap().process,
        ProcessState::Stopped
    );
    assert_eq!(
        runtime.block_on(service().start(request)).unwrap().process,
        ProcessState::Stopped
    );
    assert_eq!(proof()["starts"], 1);
    eprintln!(
        "PASS actual Service -> ExternalBackend(pulse) -> {} -> owned HTTP fixture -> {} -> daemon {}; exactly one task and acknowledgment; worker stopped",
        adapter.display(),
        shim.display(),
        daemon.address
    );
}
