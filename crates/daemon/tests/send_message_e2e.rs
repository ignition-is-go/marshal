//! End-to-end test for `SendMessage`'s happy path.
//!
//! Spins up a real `CellServer` with a WS listener, connects two
//! `MykoClient`s as A (sender) and B (recipient), each `SET`s a `Session`,
//! and then has A send `SendMessage` over the wire. The recipient
//! registers `on_command::<NotifyChannel>` and proves end-to-end that:
//!   1. the dispatch lands on B's WS connection (NotifyChannel arrives),
//!   2. only after dispatch lands does the daemon persist the `Message`
//!      (verified by inspecting the registry once B has acked receipt).
//!
//! This is the test that would have caught the silent-drop the saga path
//! suffered from when a recipient bounced between disconnect and re-SET
//! — the per-handler delivery contract makes the failure loud.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use hyphae::Gettable;
use marshal_entities::{
    GetAllSessions, Message, NotifyChannel, SendMessage, SendMessageResult, Session, SessionId,
};
use myko::{
    client::{ConnectionStatus, MykoClient, MykoProtocol},
    core::item::Eventable,
    server::{CellServerCtx, Persister},
    wire::{MEvent, MEventType},
};
use myko_server::{BlackholePersister, CellServer};
use uuid::Uuid;

const POLL_TIMEOUT: Duration = Duration::from_secs(8);

struct ServerHandle {
    ctx: CellServerCtx,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl ServerHandle {
    fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("pick free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn spawn_server(bind: SocketAddr) -> ServerHandle {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<CellServerCtx>(1);
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

    let join = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
            let server = Arc::new(
                CellServer::builder()
                    .with_bind_addr(bind)
                    .with_default_persister(blackhole)
                    .build(),
            );
            ready_tx.send(server.ctx()).expect("send ctx");
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::task::spawn_blocking(move || {
                    let _ = shutdown_rx.recv();
                }) => {}
            }
        });
        drop(rt);
    });

    let ctx = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server thread came up");

    ServerHandle {
        ctx,
        shutdown: Some(shutdown_tx),
        join: Some(join),
    }
}

fn wait_for(label: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + POLL_TIMEOUT;
    while Instant::now() < deadline {
        if f() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for: {label}");
}

fn make_session(id: &str) -> Session {
    Session {
        id: SessionId(Arc::from(id)),
        client_id: None, // server auto-populates
        pid: 0,
        cwd: "/repo".into(),
        git_branch: None,
        current_task: None,
        connected_at: chrono::Utc::now().timestamp_millis(),
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
        operator: None,
        host: None,
        project: None,
        channels_enabled: None,
    }
}

fn send_session_set(client: &MykoClient, session: &Session) {
    let event = MEvent::from_item(session, MEventType::SET, &Uuid::new_v4().to_string());
    client.send_event(event).expect("send_event");
}

fn message_count(ctx: &CellServerCtx) -> usize {
    ctx.registry
        .get(Message::ENTITY_NAME_STATIC)
        .map(|store| store.entries().get().len())
        .unwrap_or(0)
}

#[test]
fn send_message_delivers_then_persists_when_recipient_is_live() {
    let _ = env_logger::builder().is_test(true).try_init();
    marshal_entities::link();
    daemon::link();

    let port = pick_free_port();
    let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let addr = format!("ws://{bind}");
    let server = spawn_server(bind);

    // Sender (A) and recipient (B) both connect a MykoClient.
    let client_a = MykoClient::new();
    client_a.set_protocol(MykoProtocol::JSON);
    let client_b = MykoClient::new();
    client_b.set_protocol(MykoProtocol::JSON);

    // B: subscribe to NotifyChannel before connecting so no push can race
    // past us. We park the received command in a Mutex<Option<>> for
    // assertion below.
    let received: Arc<Mutex<Option<NotifyChannel>>> = Arc::new(Mutex::new(None));
    let received_for_handler = Arc::clone(&received);
    let notify_guard = client_b.on_command::<NotifyChannel, _>(move |cmd, _responder| {
        *received_for_handler.lock().expect("notify mutex") = Some(cmd);
    });
    Box::leak(Box::new(notify_guard));

    // Open a query subscription on B so the client's Session store stays
    // hot through the dispatch — without a watcher the cell can drop and
    // surprise the test.
    let _b_sessions = client_b.watch_query::<GetAllSessions>(GetAllSessions {});
    // Same on A so resolve_recipient-equivalent state can settle.
    let _a_sessions = client_a.watch_query::<GetAllSessions>(GetAllSessions {});

    client_a.set_address(Some(addr.clone()));
    client_b.set_address(Some(addr.clone()));

    let status_a = client_a.connection_status();
    let status_b = client_b.connection_status();
    {
        let s = status_a.clone();
        wait_for("A connected", move || {
            matches!(s.get(), ConnectionStatus::Connected(_))
        });
    }
    {
        let s = status_b.clone();
        wait_for("B connected", move || {
            matches!(s.get(), ConnectionStatus::Connected(_))
        });
    }

    // Give the watch_query subscriptions a moment to register on the server
    // before we publish — otherwise the SETs can fan out before A's
    // GetAllSessions subscription is wired up, leaving A blind to its own
    // SET event.
    thread::sleep(Duration::from_millis(200));

    // Each side announces its Session. The server fills in client_id.
    send_session_set(&client_a, &make_session("sess-alpha"));
    send_session_set(&client_b, &make_session("sess-bravo"));

    // Wait until both sessions are in A's view of GetAllSessions, with
    // their client_id populated by the server.
    {
        let cell = _a_sessions.clone();
        wait_for("both sessions visible to A with client_ids", move || {
            let sessions = cell.get();
            let a = sessions.iter().find(|s| s.id.0.as_ref() == "sess-alpha");
            let b = sessions.iter().find(|s| s.id.0.as_ref() == "sess-bravo");
            matches!((a, b), (Some(a), Some(b)) if a.client_id.is_some() && b.client_id.is_some())
        });
    }

    // A sends to B by session id.
    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("sess-bravo")),
        body: "hello bravo".into(),
        as_session: None, // WS path: sender resolved from the connection
    };
    let response_cell = client_a.send_command::<SendMessage, SendMessageResult>(&cmd);

    // Wait for the command response (Cell value transitions from None to
    // Some(Ok(_))).
    {
        let cell = response_cell.clone();
        wait_for("A's send_command response", move || {
            matches!(cell.get(), Some(Ok(_)))
        });
    }
    let result = response_cell.get().expect("got response").expect("ok");
    assert_eq!(result.to_session_id.0.as_ref(), "sess-bravo");

    // The NotifyChannel push should have landed on B before the response
    // returned (handler emits both inline). Don't wait long — if it isn't
    // there immediately, something dropped it.
    {
        let received = Arc::clone(&received);
        wait_for("B received NotifyChannel", move || {
            received.lock().expect("notify mutex").is_some()
        });
    }
    let push = received.lock().unwrap().take().expect("push");
    // The channel banner is a concise origin ping by the sender's nickname —
    // NOT the body (which would just be a truncated banner). The full body
    // travels in meta.body (and the persisted Message / inbox).
    assert_eq!(
        push.content,
        format!("new message from {}", marshal_entities::nickname("sess-alpha")),
        "content should be a nickname-only origin ping, got: {}",
        push.content,
    );
    assert!(
        !push.content.contains("hello bravo"),
        "content must NOT carry the body, got: {}",
        push.content,
    );
    assert_eq!(push.meta.get("body"), Some(&serde_json::json!("hello bravo")));
    assert_eq!(
        push.meta.get("kind"),
        Some(&serde_json::json!("new_message"))
    );
    assert_eq!(
        push.meta.get("from_session"),
        Some(&serde_json::json!("sess-alpha"))
    );
    assert_eq!(
        push.meta.get("from_nickname"),
        Some(&serde_json::json!(marshal_entities::nickname("sess-alpha")))
    );

    // And the server persisted exactly one Message — only after the push
    // succeeded.
    assert_eq!(
        message_count(&server.ctx),
        1,
        "Message must be persisted on successful delivery",
    );

    server.shutdown();
}

/// Regression guard for the "delivered_live lied" outage: a recipient with a
/// LIVE WS client but channels OFF (claude launched without
/// `--dangerously-load-development-channels`) silently drops channel pushes.
/// SendMessage MUST report `delivered_live == false` (queued to inbox), NOT
/// claim a live delivery that the recipient never renders — and must NOT emit
/// a push. The message still persists so the recipient pulls it next turn.
#[test]
fn live_client_with_channels_off_is_not_delivered_live() {
    let _ = env_logger::builder().is_test(true).try_init();
    marshal_entities::link();
    daemon::link();

    let port = pick_free_port();
    let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let addr = format!("ws://{bind}");
    let server = spawn_server(bind);

    let client_a = MykoClient::new();
    client_a.set_protocol(MykoProtocol::JSON);
    let client_b = MykoClient::new();
    client_b.set_protocol(MykoProtocol::JSON);

    // B watches for any NotifyChannel push — there must be NONE.
    let received: Arc<Mutex<Option<NotifyChannel>>> = Arc::new(Mutex::new(None));
    let received_for_handler = Arc::clone(&received);
    let notify_guard = client_b.on_command::<NotifyChannel, _>(move |cmd, _responder| {
        *received_for_handler.lock().expect("notify mutex") = Some(cmd);
    });
    Box::leak(Box::new(notify_guard));

    let _b_sessions = client_b.watch_query::<GetAllSessions>(GetAllSessions {});
    let _a_sessions = client_a.watch_query::<GetAllSessions>(GetAllSessions {});

    client_a.set_address(Some(addr.clone()));
    client_b.set_address(Some(addr.clone()));
    {
        let s = client_a.connection_status();
        wait_for("A connected", move || {
            matches!(s.get(), ConnectionStatus::Connected(_))
        });
    }
    {
        let s = client_b.connection_status();
        wait_for("B connected", move || {
            matches!(s.get(), ConnectionStatus::Connected(_))
        });
    }
    thread::sleep(Duration::from_millis(200));

    // B announces a session that is LIVE but channels-OFF.
    let mut b = make_session("sess-bravo-off");
    b.channels_enabled = Some(false);
    send_session_set(&client_a, &make_session("sess-alpha"));
    send_session_set(&client_b, &b);
    {
        let cell = _a_sessions.clone();
        wait_for("both sessions visible with client_ids", move || {
            let s = cell.get();
            let a = s.iter().find(|x| x.id.0.as_ref() == "sess-alpha");
            let b = s.iter().find(|x| x.id.0.as_ref() == "sess-bravo-off");
            matches!((a, b), (Some(a), Some(b)) if a.client_id.is_some() && b.client_id.is_some())
        });
    }

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("sess-bravo-off")),
        body: "to a flag-off live client".into(),
        as_session: None,
    };
    let response_cell = client_a.send_command::<SendMessage, SendMessageResult>(&cmd);
    {
        let cell = response_cell.clone();
        wait_for("send_command response", move || {
            matches!(cell.get(), Some(Ok(_)))
        });
    }
    let result = response_cell.get().expect("response").expect("ok");

    // The point of the fix: a live-but-flag-off client is NOT a live delivery.
    assert!(
        !result.delivered_live,
        "channels-off recipient must report delivered_live=false (queued to inbox), not a phantom live delivery"
    );
    // No push should have been emitted (it'd be dropped anyway).
    thread::sleep(Duration::from_millis(200));
    assert!(
        received.lock().unwrap().is_none(),
        "no channel push should be sent to a channels-off recipient"
    );
    // But it MUST persist so the recipient pulls it from the inbox next turn.
    assert_eq!(
        message_count(&server.ctx),
        1,
        "message must persist for inbox pull even when not delivered live",
    );

    server.shutdown();
}
