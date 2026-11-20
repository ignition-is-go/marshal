#![cfg(unix)]

use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use marshal_launch::api::{
    self, GET_LAUNCH_PATH, LAUNCH_PATH, LaunchKey, LaunchView, ProcessState, STOP_LAUNCH_PATH,
    StartLaunch, TaskState,
};
use serde_json::json;

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

#[tokio::test]
async fn recovered_start_keeps_its_worker_and_delivers_one_task() {
    let root = tempfile::tempdir().unwrap();
    let backend = root.path().join("marshal-launch-fixture");
    std::fs::write(&backend, r#"#!/bin/sh
cat >/dev/null
case "$1" in
    check)
        if [ -f "$FIXTURE/hold-preflight" ]; then
            touch "$FIXTURE/in-preflight"
            while [ -f "$FIXTURE/hold-preflight" ]; do sleep 0.01; done
        fi
        printf '{"version":2}\n'
        ;;
    recover) printf '{"version":2,"handle":"fixture-worker"}\n' ;;
    launch)
        printf 'started\n' >> "$FIXTURE/attempts"; touch "$FIXTURE/running"
        [ -f "$FIXTURE/confirm-start" ] || exit 7
        printf '{"version":2,"handle":"fixture-worker","host":"worker-node","cwd":"/work","where":"fixture"}\n'
        ;;
    status)
        if [ -f "$FIXTURE/running" ]; then running=true; else running=false; fi
        printf '{"version":2,"running":%s}\n' "$running"
        ;;
    inspect)
        printf '{"version":2,"launched":{"handle":"fixture-worker","host":"worker-node","cwd":"/work","where":"fixture"}}\n'
        ;;
    stop)
        [ ! -f "$FIXTURE/stop-unavailable" ] || exit 9
        if [ -f "$FIXTURE/running" ]; then
            rm "$FIXTURE/running"; printf '{"version":2,"state":"stopped"}\n'
        else
            printf '{"version":2,"state":"gone"}\n'
        fi
        ;;
    *) exit 7 ;;
esac
"#).unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    let ws = free_address();
    let hooks = free_address();
    assert_ne!(ws, hooks);
    let worker_path = std::env::join_paths(std::iter::once(root.path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let start_daemon = || async {
        let daemon = Daemon(
            Command::new(env!("CARGO_BIN_EXE_marshal-daemon"))
                .env_clear()
                .env("PATH", &worker_path)
                .env("FIXTURE", root.path())
                .env("MARSHAL_LAUNCH_STATE", root.path().join("state"))
                .env("MARSHAL_LAUNCH_ADDRESS", "ws://marshal.example:6155")
                .env("MARSHAL_BIND", ws.to_string())
                .env("MARSHAL_HOOK_BIND", hooks.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        tokio::time::timeout(Duration::from_secs(8), async {
            while tokio::net::TcpStream::connect(hooks).await.is_err() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        daemon
    };
    let daemon = start_daemon().await;
    let base = format!("http://{hooks}");
    let request = StartLaunch {
        launch_id: marshal_launch::new_agent_id(),
        backend: "fixture".into(),
        harness: "claude".into(),
        cwd: "/work".into(),
        name: "worker".into(),
        options: vec![],
        check: true,
        timeout_secs: 3,
        first_task: Some("handle this exactly once".into()),
    };
    let key = LaunchKey {
        launch_id: request.launch_id.clone(),
    };
    let uncertain: LaunchView = api::call(&base, LAUNCH_PATH, &request).await.unwrap();
    assert_eq!(uncertain.process, ProcessState::Unknown);
    assert!(uncertain.error.is_some());
    assert!(root.path().join("running").exists());
    drop(daemon);
    let daemon = start_daemon().await;
    let observed: LaunchView = api::call(&base, GET_LAUNCH_PATH, &key).await.unwrap();
    assert_eq!(observed.process, ProcessState::Running);

    let client = reqwest::Client::new();
    let payload = json!({"session_id":"worker-session","cwd":"/work","agent_id":request.launch_id});
    let register = || {
        client
            .post(format!(
                "{base}/hook/session-register?host=worker-node&harness=claude"
            ))
            .body(payload.to_string())
            .send()
    };
    register().await.unwrap().error_for_status().unwrap();
    let recovered: LaunchView = api::call(&base, LAUNCH_PATH, &request).await.unwrap();
    assert!(recovered.error.is_none(), "{recovered:?}");
    assert_eq!(recovered.session_id.as_deref(), Some("worker-session"));
    let TaskState::Stored { message_id } = &recovered.task else {
        panic!("first task was not stored: {recovered:?}");
    };
    let journal = root
        .path()
        .join("state")
        .join(format!("{}.json", key.launch_id));
    let uncertain_task = TaskState::DeliveryUncertain {
        message_id: message_id.clone(),
    };
    let lose_send_reply = || {
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
        record["view"]["task"] = serde_json::to_value(&uncertain_task).unwrap();
        std::fs::write(&journal, serde_json::to_vec(&record).unwrap()).unwrap();
    };
    lose_send_reply();
    let reconciled: LaunchView = api::call(&base, GET_LAUNCH_PATH, &key).await.unwrap();
    assert_eq!(reconciled.task, recovered.task);
    let different = StartLaunch {
        cwd: "/another-project".into(),
        ..request.clone()
    };
    let rejected = api::call::<_, LaunchView>(&base, LAUNCH_PATH, &different)
        .await
        .unwrap_err();
    assert!(
        rejected.to_string().contains("different request"),
        "{rejected:#}"
    );
    let repeated: LaunchView = api::call(&base, LAUNCH_PATH, &request).await.unwrap();
    assert_eq!(repeated.task, recovered.task);
    let reply = client
        .post(format!(
            "{base}/hook/prompt-submit?host=worker-node&harness=claude"
        ))
        .body(payload.to_string())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        reply.matches("handle this exactly once").count(),
        1,
        "{reply}"
    );
    let confirmed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view: LaunchView = api::call(&base, GET_LAUNCH_PATH, &key).await.unwrap();
            if matches!(view.task, TaskState::Acknowledged { .. }) {
                break view;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(confirmed.session_id, repeated.session_id);
    assert_eq!(
        std::fs::read_to_string(root.path().join("attempts")).unwrap(),
        "started\n"
    );

    assert_eq!(
        std::fs::read_dir(root.path().join("state"))
            .unwrap()
            .count(),
        1
    );
    lose_send_reply();
    let reconciled: LaunchView = api::call(&base, GET_LAUNCH_PATH, &key).await.unwrap();
    assert_eq!(reconciled.task, confirmed.task);

    drop(daemon);
    lose_send_reply();
    let _daemon = start_daemon().await;
    register().await.unwrap().error_for_status().unwrap();
    let retry: LaunchView = api::call(&base, LAUNCH_PATH, &request).await.unwrap();
    assert_eq!(retry.task, uncertain_task);
    let reply = client
        .post(format!(
            "{base}/hook/prompt-submit?host=worker-node&harness=claude"
        ))
        .body(payload.to_string())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!reply.contains("handle this exactly once"), "{reply}");

    let stop_unavailable = root.path().join("stop-unavailable");
    std::fs::write(&stop_unavailable, "").unwrap();
    let failed_stop: LaunchView = api::call(&base, STOP_LAUNCH_PATH, &key).await.unwrap();
    assert_eq!(failed_stop.process, ProcessState::Unknown);
    assert!(failed_stop.error.is_some());
    assert!(root.path().join("running").exists());
    std::fs::remove_file(stop_unavailable).unwrap();

    for _ in 0..2 {
        let stopped: LaunchView = api::call(&base, STOP_LAUNCH_PATH, &key).await.unwrap();
        assert_eq!(stopped.process, ProcessState::Stopped);
    }
    assert!(!root.path().join("running").exists());

    // If stop sees no worker before the backend actually finishes startup,
    // the stop stays retryable and a repeated start cannot resume handoff.
    let request = StartLaunch {
        launch_id: marshal_launch::new_agent_id(),
        ..request
    };
    let key = LaunchKey {
        launch_id: request.launch_id.clone(),
    };
    let uncertain: LaunchView = api::call(&base, LAUNCH_PATH, &request).await.unwrap();
    assert_eq!(uncertain.process, ProcessState::Unknown);
    std::fs::remove_file(root.path().join("running")).unwrap();
    let not_yet_present: LaunchView = api::call(&base, STOP_LAUNCH_PATH, &key).await.unwrap();
    assert_eq!(not_yet_present.process, ProcessState::Unknown);
    std::fs::write(root.path().join("running"), "").unwrap();
    let replayed_start: LaunchView = api::call(&base, LAUNCH_PATH, &request).await.unwrap();
    assert!(replayed_start.session_id.is_none());
    let stopped: LaunchView = api::call(&base, STOP_LAUNCH_PATH, &key).await.unwrap();
    assert_eq!(stopped.process, ProcessState::Stopped);

    let attempts = root.path().join("attempts");
    let before_preflight_stop = std::fs::read(&attempts).unwrap();
    let hold_preflight = root.path().join("hold-preflight");
    std::fs::write(&hold_preflight, "").unwrap();
    let cancelled_request = StartLaunch {
        launch_id: marshal_launch::new_agent_id(),
        ..request.clone()
    };
    let cancelled_key = LaunchKey {
        launch_id: cancelled_request.launch_id.clone(),
    };
    let start = api::call::<_, LaunchView>(&base, LAUNCH_PATH, &cancelled_request);
    let cancel_preflight = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !root.path().join("in-preflight").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let stop = api::call::<_, LaunchView>(&base, STOP_LAUNCH_PATH, &cancelled_key);
        tokio::pin!(stop);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut stop)
                .await
                .is_err()
        );
        std::fs::remove_file(&hold_preflight).unwrap();
        stop.await.unwrap()
    };
    let (interrupted, stopped) = tokio::join!(start, cancel_preflight);
    assert_eq!(
        std::fs::read(&attempts).unwrap(),
        before_preflight_stop,
        "stop during preflight must prevent the backend launch call"
    );
    assert_eq!(stopped.process, ProcessState::Stopped);
    assert!(
        interrupted
            .unwrap()
            .error
            .unwrap()
            .contains("stop requested")
    );

    // Stop cancels a long roster wait instead of waiting for the join deadline.
    std::fs::write(root.path().join("confirm-start"), "").unwrap();
    let request = StartLaunch {
        launch_id: marshal_launch::new_agent_id(),
        timeout_secs: 120,
        ..request
    };
    let key = LaunchKey {
        launch_id: request.launch_id.clone(),
    };
    let start_base = base.clone();
    let start = tokio::spawn(async move {
        api::call::<_, LaunchView>(&start_base, LAUNCH_PATH, &request).await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.path().join("running").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let stopped: LaunchView = tokio::time::timeout(
        Duration::from_secs(5),
        api::call(&base, STOP_LAUNCH_PATH, &key),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(stopped.process, ProcessState::Stopped);
    let interrupted = start.await.unwrap().unwrap();
    assert!(interrupted.error.unwrap().contains("stop requested"));
}
