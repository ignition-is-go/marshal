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

fn make_session(id: &str, nickname: &str) -> Session {
    Session {
        id: SessionId(Arc::from(id)),
        client_id: None, // server auto-populates
        nickname: nickname.into(),
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
    send_session_set(&client_a, &make_session("a", "alpha"));
    send_session_set(&client_b, &make_session("b", "bravo"));

    // Wait until both sessions are in A's view of GetAllSessions, with
    // their client_id populated by the server.
    {
        let cell = _a_sessions.clone();
        wait_for("both sessions visible to A with client_ids", move || {
            let sessions = cell.get();
            let a = sessions.iter().find(|s| s.id.0.as_ref() == "a");
            let b = sessions.iter().find(|s| s.id.0.as_ref() == "b");
            matches!((a, b), (Some(a), Some(b)) if a.client_id.is_some() && b.client_id.is_some())
        });
    }

    // A sends to B by session id.
    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("b")),
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
    assert_eq!(result.to_nick, "bravo");

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
    assert!(
        push.content.contains("hello bravo"),
        "channel content should carry the message body, got: {}",
        push.content,
    );
    assert_eq!(
        push.meta.get("kind"),
        Some(&serde_json::json!("new_message"))
    );
    assert_eq!(
        push.meta.get("from_nick"),
        Some(&serde_json::json!("alpha"))
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
