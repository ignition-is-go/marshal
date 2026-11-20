use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime},
};

use hyphae::Gettable;
use marshal_client::host::{ExecutionRequest, ExecutionResult, Harness, HostConnection};
use marshal_client::{ConnectionStatus, MarshalClient};
use marshal_entities::{
    AcceptTaskAssignment, AckMessages, CompleteTaskAssignment, CreateTask, DispatchHarness,
    DispatchState, GetAllDispatchAttempts, GetAllDispatchHosts, GetAllMessages, GetAllTaskClaims,
    GetTaskById, ListReadyTasks, ReserveTaskDispatch, Session, SessionId, TaskPriority, TaskStatus,
};
use marshal_server::{
    dispatch::reconcile_once,
    hosts::{HostRegistry, serve},
};
use myko::{
    command::{CommandContext, CommandHandler},
    prelude::EventPublishing as _,
    request::RequestContext,
};
use myko_auth::{
    AuthError, AuthSession, AuthenticatedIdentity, AuthorizationRequest, Authorizer, Principal,
    PrincipalKind, identity::UserId,
};
use myko_server::{BlackholePersister, MykoServer};
use tokio::{net::TcpListener, sync::mpsc};

struct TestOperator;

impl Authorizer for TestOperator {
    fn authorize(
        &self,
        _: &AuthenticatedIdentity,
        _: AuthorizationRequest<'_>,
    ) -> Result<(), AuthError> {
        Ok(())
    }
}

#[tokio::test]
async fn dispatch_reserves_once_and_recovers_a_lost_launch_reply_by_inspection() {
    verify_dispatch(false).await;
}

#[tokio::test]
async fn managed_dispatch_requires_matching_guest_evidence_before_delivery() {
    verify_dispatch(true).await;
}

async fn verify_dispatch(managed: bool) {
    marshal_entities::link();
    let bind = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let server = MykoServer::builder()
        .with_bind_addr(bind)
        .with_default_persister(Arc::new(BlackholePersister))
        .build();
    let context = Arc::new(server.ctx());
    let myko_server = tokio::spawn(async move { server.run().await.unwrap() });
    let operator = AuthSession::new(
        AuthenticatedIdentity {
            user_id: Some(UserId("operator".into())),
            principal: Principal {
                issuer: "test".into(),
                subject: "operator".into(),
                kind: PrincipalKind::User,
            },
            client_id: None,
            scopes: vec![],
            delegation: vec![],
            claims: serde_json::Value::Null,
            expires_at: SystemTime::now() + Duration::from_secs(60),
        },
        Arc::new(TestOperator),
    );
    let command = |auth| {
        let req = RequestContext::internal(
            uuid::Uuid::new_v4().to_string().into(),
            context.host_id,
            "test",
        )
        .with_auth(auth);
        CommandContext::new(Arc::from("test"), Arc::new(req), context.clone())
    };
    let task_id = CreateTask {
        project: "marshal".into(),
        title: "Run a worker".into(),
        body: String::new(),
        priority: TaskPriority::P1,
        labels: vec![],
    }
    .execute(command(None))
    .unwrap();
    let reserve = || ReserveTaskDispatch {
        task_id: task_id.clone(),
        host_id: "workstation".into(),
        harness: DispatchHarness::Codex,
        workspace: "/work/marshal".into(),
    };
    assert!(reserve().execute(command(None)).is_err());
    let mut rejected = reserve().execute(command(Some(operator.clone()))).unwrap();
    rejected.state = DispatchState::Rejected {
        reason: "Host rejected before launch".into(),
    };
    command(None).emit_set(&rejected).unwrap();
    assert_eq!(
        ListReadyTasks { project: None }
            .execute(command(None))
            .unwrap(),
        vec![task_id.clone()]
    );
    let attempt = reserve().execute(command(Some(operator.clone()))).unwrap();
    assert_ne!(rejected.launch_id, attempt.launch_id);
    let retry = reserve().execute(command(Some(operator.clone()))).unwrap();
    assert_eq!(attempt.launch_id, retry.launch_id);
    let states = || {
        command(None)
            .exec_query(GetAllDispatchAttempts {})
            .unwrap()
            .into_iter()
            .filter(|row| row.task_id == task_id)
            .collect::<Vec<_>>()
    };
    assert_eq!(states()[0].state, DispatchState::Reserved);
    assert!(
        ListReadyTasks { project: None }
            .execute(command(None))
            .unwrap()
            .is_empty()
    );

    let registry = HostRegistry::with_roster(context.clone());
    reconcile_once(&context, &registry).await.unwrap();
    assert_eq!(states()[0].state, DispatchState::Reserved);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/host", listener.local_addr().unwrap());
    let token = "a-long-random-host-token-for-this-test";
    let host_server = tokio::spawn(serve(
        listener,
        registry.clone(),
        HashMap::from([
            ("workstation".into(), token.into()),
            (
                "second-host".into(),
                "a-different-token-for-the-second-host".into(),
            ),
        ]),
    ));

    let mut host = HostConnection::connect(&url, "workstation", token, vec![Harness::Codex])
        .await
        .unwrap();
    let (requests, mut received) = mpsc::channel(1);
    let first = tokio::spawn(async move {
        host.run(move |request| {
            let requests = requests.clone();
            async move {
                requests.send(request).await.unwrap();
                std::future::pending().await
            }
        })
        .await
    });
    let second_task = CreateTask {
        project: "marshal".into(),
        title: "Launch on the other host".into(),
        body: String::new(),
        priority: TaskPriority::P1,
        labels: vec![],
    }
    .execute(command(None))
    .unwrap();
    ReserveTaskDispatch {
        task_id: second_task.clone(),
        host_id: "second-host".into(),
        ..reserve()
    }
    .execute(command(Some(operator.clone())))
    .unwrap();
    let mut second = HostConnection::connect(
        &url,
        "second-host",
        "a-different-token-for-the-second-host",
        vec![Harness::Codex],
    )
    .await
    .unwrap();
    let published = command(None).exec_query(GetAllDispatchHosts {}).unwrap();
    assert!(published.iter().any(|host| {
        host.id.0.as_ref() == "second-host" && host.launch_harnesses == vec![DispatchHarness::Codex]
    }));
    let second = tokio::spawn(async move {
        second
            .run(|request| async move {
                match request {
                    ExecutionRequest::Start { .. } if managed => ExecutionResult::Running {
                        handle: "terminal-guest".into(),
                    },
                    ExecutionRequest::Inspect { .. } if managed => ExecutionResult::Failed {
                        reason: "managed launch failed".into(),
                    },
                    _ => ExecutionResult::Rejected {
                        reason: "preflight refused".into(),
                    },
                }
            })
            .await
    });
    let round = {
        let (context, registry) = (context.clone(), registry.clone());
        tokio::spawn(async move { reconcile_once(&context, &registry).await })
    };
    let started = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        started,
        ExecutionRequest::Start {
            launch_id: attempt.launch_id.clone(),
            harness: Harness::Codex,
            workspace: "/work/marshal".into()
        }
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let all = command(None).exec_query(GetAllDispatchAttempts {}).unwrap();
            if all.iter().any(|attempt| {
                attempt.task_id == second_task
                    && if managed {
                        matches!(attempt.state, DispatchState::Running { .. })
                    } else {
                        matches!(attempt.state, DispatchState::Rejected { .. })
                    }
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("another host must finish while the first host is still waiting");
    first.abort();
    tokio::time::timeout(Duration::from_secs(5), round)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(states()[0].state, DispatchState::Starting);

    let mut host = HostConnection::connect(&url, "workstation", token, vec![Harness::Codex])
        .await
        .unwrap();
    let expected = attempt.launch_id.clone();
    let attach = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let host_attach = attach.clone();
    let accept = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let host_accept = accept.clone();
    let host_task = task_id.to_string();
    let recovery = tokio::spawn(async move {
        host.run(move |request| {
            let expected = expected.clone();
            let attach = host_attach.clone();
            let accept = host_accept.clone();
            let task_id = host_task.clone();
            async move {
                assert_eq!(
                    request,
                    ExecutionRequest::Inspect {
                        launch_id: expected
                    }
                );
                let acceptance = accept.load(std::sync::atomic::Ordering::Relaxed);
                if acceptance != 0 {
                    ExecutionResult::Accepted {
                        handle: "recovered-worker".into(),
                        session_id: "joined-session".into(),
                        task_id: if acceptance == 1 {
                            "wrong-task".into()
                        } else {
                            task_id
                        },
                    }
                } else if attach.load(std::sync::atomic::Ordering::Relaxed) {
                    if managed {
                        ExecutionResult::Managed {
                            handle: "recovered-worker".into(),
                            host: "guest.example.test".into(),
                            cwd: "/work/marshal".into(),
                        }
                    } else {
                        ExecutionResult::Attached {
                            handle: "recovered-worker".into(),
                            session_id: "joined-session".into(),
                        }
                    }
                } else {
                    ExecutionResult::Running {
                        handle: "recovered-worker".into(),
                    }
                }
            }
        })
        .await
    });
    reconcile_once(&context, &registry).await.unwrap();
    assert_eq!(
        states()[0].state,
        DispatchState::Running {
            handle: "recovered-worker".into()
        }
    );
    if managed {
        let attempts = command(None).exec_query(GetAllDispatchAttempts {}).unwrap();
        assert!(attempts.iter().any(|attempt| {
            attempt.task_id == second_task
                && matches!(attempt.state, DispatchState::Rejected { .. })
        }));
    }
    second.abort();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let hosts = command(None).exec_query(GetAllDispatchHosts {}).unwrap();
            if !hosts.iter().any(|host| host.id.0.as_ref() == "second-host") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("disconnected host must leave the roster");
    attach.store(true, std::sync::atomic::Ordering::Relaxed);
    reconcile_once(&context, &registry).await.unwrap();
    assert!(matches!(states()[0].state, DispatchState::Running { .. }));
    let mut session: Session = serde_json::from_value(serde_json::json!({
        "id": "joined-session", "pid": 0, "cwd": "/work/marshal", "connectedAt": 1,
        "agentId": "unrelated-launch"
    }))
    .unwrap();
    command(None).emit_set(&session).unwrap();
    reconcile_once(&context, &registry).await.unwrap();
    assert!(matches!(states()[0].state, DispatchState::Running { .. }));
    session.agent_id = Some(attempt.launch_id.clone());
    if managed {
        command(None).emit_set(&session).unwrap();
        reconcile_once(&context, &registry).await.unwrap();
        assert!(matches!(states()[0].state, DispatchState::Running { .. }));

        session.host = Some(marshal_entities::HostInfo {
            name: "guest".into(),
            ..Default::default()
        });
        session.cwd = "/work/other".into();
        command(None).emit_set(&session).unwrap();
        reconcile_once(&context, &registry).await.unwrap();
        assert!(matches!(states()[0].state, DispatchState::Running { .. }));

        session.cwd = "/work/marshal".into();
        let mut duplicate = session.clone();
        duplicate.id = SessionId(Arc::from("duplicate-session"));
        command(None).emit_set(&duplicate).unwrap();
        command(None).emit_set(&session).unwrap();
        reconcile_once(&context, &registry).await.unwrap();
        assert!(matches!(states()[0].state, DispatchState::Running { .. }));
        command(None).emit_del(&duplicate).unwrap();
    }
    command(None).emit_set(&session).unwrap();
    reconcile_once(&context, &registry).await.unwrap();
    assert_eq!(
        states()[0].state,
        DispatchState::Joined {
            handle: "recovered-worker".into(),
            session_id: SessionId(Arc::from("joined-session")),
        }
    );
    reconcile_once(&context, &registry).await.unwrap();
    assert!(matches!(states()[0].state, DispatchState::Offered { .. }));
    reconcile_once(&context, &registry).await.unwrap();
    let messages = command(None).exec_query(GetAllMessages {}).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].from_session_id.0.as_ref(),
        marshal_entities::DISPATCH_SENDER
    );
    assert_eq!(messages[0].to_session_id.as_ref(), Some(&session.id));
    assert!(messages[0].body.contains("Run a worker"));
    AckMessages {
        message_ids: vec![messages[0].id.clone()],
        as_session: Some(session.id.clone()),
    }
    .execute(command(None))
    .unwrap();
    accept.store(1, std::sync::atomic::Ordering::Relaxed);
    reconcile_once(&context, &registry).await.unwrap();
    assert!(matches!(states()[0].state, DispatchState::Delivered { .. }));
    let claims = || command(None).exec_query(GetAllTaskClaims {}).unwrap();
    assert!(claims().is_empty());
    if managed {
        assert!(
            marshal_entities::configure_assignment_credentials(HashMap::from([(
                "workstation".into(),
                token.into(),
            )]))
            .is_ok()
        );
        let good_proof = marshal_entities::assignment_proof(token, &attempt.launch_id);
        let accept_assignment = |proof, session_id| AcceptTaskAssignment {
            task_id: task_id.clone(),
            launch_id: attempt.launch_id.clone(),
            session_id,
            proof,
        };
        let client = MarshalClient::new();
        client.set_address(Some(format!("ws://{bind}")));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(
                client.connection_status().get(),
                ConnectionStatus::Connected(_)
            ) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("connect assignment client");

        async fn accept(
            client: &MarshalClient,
            assignment: AcceptTaskAssignment,
        ) -> Result<(), String> {
            let response = client.send_command::<AcceptTaskAssignment, ()>(&assignment);
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(result) = response.get() {
                        return result;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("wait for acceptance result")
        }
        assert!(
            accept(
                &client,
                accept_assignment("0".repeat(64), session.id.clone())
            )
            .await
            .is_err()
        );
        assert!(
            accept(
                &client,
                accept_assignment(
                    good_proof.clone(),
                    SessionId(Arc::from("different-session"))
                )
            )
            .await
            .is_err()
        );
        assert!(claims().is_empty());
        accept(
            &client,
            accept_assignment(good_proof.clone(), session.id.clone()),
        )
        .await
        .unwrap();
        let leased = claims()[0].lease_id.clone();
        accept(&client, accept_assignment(good_proof, session.id.clone()))
            .await
            .unwrap();
        assert_eq!(claims()[0].lease_id, leased);
    } else {
        accept.store(2, std::sync::atomic::Ordering::Relaxed);
        reconcile_once(&context, &registry).await.unwrap();
    }
    let recorded = claims();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].task_id, task_id);
    assert_eq!(recorded[0].session_id, session.id);
    assert_eq!(
        states()[0].state,
        DispatchState::Claimed {
            handle: "recovered-worker".into(),
            session_id: session.id.clone(),
            lease_id: recorded[0].lease_id.clone(),
        }
    );
    reconcile_once(&context, &registry).await.unwrap();
    assert_eq!(claims()[0].lease_id, recorded[0].lease_id);
    let active = reserve().execute(command(Some(operator.clone()))).unwrap();
    assert_eq!(active.launch_id, attempt.launch_id);
    if managed {
        let complete = |session_id| CompleteTaskAssignment {
            task_id: task_id.clone(),
            launch_id: attempt.launch_id.clone(),
            session_id,
            proof: marshal_entities::assignment_proof(token, &attempt.launch_id),
            commit: Some("verified-commit".into()),
        };
        let status = || {
            command(None)
                .exec_report(GetTaskById {
                    id: task_id.clone(),
                })
                .unwrap()
                .unwrap()
                .status
                .clone()
        };
        assert!(
            complete(SessionId("unrelated-session".into()))
                .execute(command(None))
                .is_err()
        );
        let mut expired = recorded[0].as_ref().clone();
        expired.expires = 0;
        command(None).emit_set(&expired).unwrap();
        assert!(complete(session.id.clone()).execute(command(None)).is_err());
        assert_eq!(status(), TaskStatus::Open);
        command(None).emit_set(recorded[0].as_ref()).unwrap();
        complete(session.id.clone()).execute(command(None)).unwrap();
        complete(session.id.clone()).execute(command(None)).unwrap();
        assert!(claims().is_empty());
        assert_eq!(
            status(),
            TaskStatus::Closed {
                commit: Some("verified-commit".into())
            }
        );
    } else {
        let mut expired = recorded[0].as_ref().clone();
        expired.expires = 0;
        command(None).emit_set(&expired).unwrap();
        let replacement = reserve().execute(command(Some(operator))).unwrap();
        assert_ne!(replacement.launch_id, attempt.launch_id);
        assert_eq!(replacement.state, DispatchState::Reserved);
    }
    recovery.abort();
    host_server.abort();
    myko_server.abort();
}
