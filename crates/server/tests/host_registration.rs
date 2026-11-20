use std::{collections::HashMap, time::Duration};

use marshal_client::host::{ExecutionRequest, ExecutionResult, Harness, HostConnection};
use marshal_server::hosts::{HostRegistry, serve};
use tokio::net::TcpListener;

#[tokio::test]
async fn host_launch_capabilities_are_opt_in_and_follow_the_current_connection() {
    let registry = HostRegistry::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    let secret = "a-long-random-host-token-for-this-test";
    let server = tokio::spawn(serve(
        listener,
        registry.clone(),
        HashMap::from([("workstation".to_owned(), secret.to_owned())]),
    ));

    assert!(
        HostConnection::connect(&url, "workstation", "wrong", vec![Harness::Codex])
            .await
            .is_err()
    );
    assert!(registry.snapshot().await.is_empty());

    let mut original = HostConnection::connect(&url, "workstation", secret, vec![Harness::Codex])
        .await
        .unwrap();
    assert_eq!(
        registry.snapshot().await["workstation"].launch_harnesses,
        vec![Harness::Codex]
    );
    let mut delivery_only = HostConnection::connect(&url, "workstation", secret, vec![])
        .await
        .unwrap();

    assert!(original.heartbeat().await.is_err());
    drop(original);
    delivery_only.heartbeat().await.unwrap();
    assert!(
        registry.snapshot().await["workstation"]
            .launch_harnesses
            .is_empty()
    );

    drop(delivery_only);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !registry.snapshot().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    server.abort();
}

#[tokio::test]
async fn routes_execution_to_the_opted_in_host() {
    let registry = HostRegistry::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    let secret = "a-long-random-host-token-for-this-test";
    let server = tokio::spawn(serve(
        listener,
        registry.clone(),
        HashMap::from([("workstation".to_owned(), secret.to_owned())]),
    ));
    let mut host = HostConnection::connect(&url, "workstation", secret, vec![Harness::Codex])
        .await
        .unwrap();
    let runner = tokio::spawn(async move {
        host.run(async |request| match request {
            ExecutionRequest::Start { launch_id, .. } => {
                ExecutionResult::Running { handle: launch_id }
            }
            ExecutionRequest::Inspect { .. } => ExecutionResult::Unknown,
            ExecutionRequest::Stop { .. } => ExecutionResult::Stopped,
        })
        .await
    });
    let start = |harness| ExecutionRequest::Start {
        launch_id: "launch".into(),
        harness,
        workspace: "/workspace".into(),
    };
    assert!(
        registry
            .execute("workstation", start(Harness::Claude))
            .await
            .is_err()
    );
    let (start_result, inspect_result) = tokio::join!(
        registry.execute("workstation", start(Harness::Codex)),
        registry.execute(
            "workstation",
            ExecutionRequest::Inspect {
                launch_id: "launch".into()
            }
        ),
    );
    assert_eq!(
        start_result.unwrap(),
        ExecutionResult::Running {
            handle: "launch".into()
        }
    );
    assert_eq!(inspect_result.unwrap(), ExecutionResult::Unknown);
    assert_eq!(
        registry
            .execute(
                "workstation",
                ExecutionRequest::Stop {
                    launch_id: "launch".into()
                }
            )
            .await
            .unwrap(),
        ExecutionResult::Stopped
    );
    runner.abort();
    server.abort();
}
