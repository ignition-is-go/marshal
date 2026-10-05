//! Regression test for ignition-is-go/marshal#130: a direct message's read
//! mark must survive its recipient session being DEL'd and re-SET under the
//! same id (a pi `/reload` or restart).
//!
//! Sequence: SET S, DM S, ack as S, DEL S, SET S, read unread for S.

use std::{
    net::SocketAddr,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use hyphae::{Gettable, Materialize};
use marshal_entities::{
    AckMessages, AckMessagesResult, GetAllSessions, Message, MessageId, MessageRead,
    ReadMessages, ReadMessagesResult, RoomMember, SendMessage, SendMessageResult, Session,
    SessionId,
};
use myko::{
    client::{ConnectionStatus, MykoClient, MykoProtocol},
    prelude::Eventable,
    server::{MykoServerContext, Persister},
    wire::{MEvent, MEventType},
};
use myko_server::{BlackholePersister, MykoServer};
use uuid::Uuid;

const POLL_TIMEOUT: Duration = Duration::from_secs(8);

struct ServerHandle {
    ctx: MykoServerContext,
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
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<MykoServerContext>(1);
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

    let join = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
            let server = Arc::new(
                MykoServer::builder()
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
        client_id: None,
        pid: 0,
        cwd: "/repo".into(),
        git_branch: None,
        current_task: None,
        session_name: None,
        activity: None,
        kind: None,
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
    client.send_event(event).expect("send_event SET");
}

fn send_session_del(client: &MykoClient, session: &Session) {
    let event = MEvent::from_item(session, MEventType::DEL, &Uuid::new_v4().to_string());
    client.send_event(event).expect("send_event DEL");
}

/// Count items in a registry store by entity name.
fn store_count(ctx: &MykoServerContext, entity: &str) -> usize {
    ctx.registry
        .get(entity)
        .map(|s| s.entries().materialize().get().len())
        .unwrap_or(0)
}

/// The single Message's id, or None.
fn first_message_id(ctx: &MykoServerContext) -> Option<Arc<str>> {
    let store = ctx.registry.get(Message::ENTITY_NAME_STATIC)?;
    store.entries().materialize().get().into_iter().next().map(|(k, _)| k)
}

/// Authoritative server-side: is S present in the Session store?
fn session_in_server(ctx: &MykoServerContext, id: &str) -> bool {
    let Some(store) = ctx.registry.get(Session::ENTITY_NAME_STATIC) else {
        return false;
    };
    store
        .entries()
        .materialize()
        .get()
        .into_iter()
        .any(|(k, _)| k.as_ref() == id)
}

#[test]
fn del_session_then_reregister_does_not_reinject_acked_direct_dm() {
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

    let _a_sessions = client_a.watch_query::<GetAllSessions>(GetAllSessions {});
    let _b_sessions = client_b.watch_query::<GetAllSessions>(GetAllSessions {});

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

    let s = make_session("sess-bravo");
    send_session_set(&client_a, &make_session("sess-alpha"));
    send_session_set(&client_b, &s);
    {
        let cell = _a_sessions.clone();
        wait_for("both sessions visible with client_ids", move || {
            let sess = cell.get();
            let a = sess.iter().find(|x| x.id.0.as_ref() == "sess-alpha");
            let b = sess.iter().find(|x| x.id.0.as_ref() == "sess-bravo");
            matches!((a, b), (Some(a), Some(b)) if a.client_id.is_some() && b.client_id.is_some())
        });
    }

    // 1. A sends a DIRECT DM to B.
    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("sess-bravo")),
        body: "the one and only direct DM".into(),
        as_session: None,
    };
    let response_cell = client_a.send_command::<SendMessage, SendMessageResult>(&cmd);
    {
        let cell = response_cell.clone();
        wait_for("A's send response", move || matches!(cell.get(), Some(Ok(_))));
    }
    wait_for("DM persisted", || store_count(&server.ctx, Message::ENTITY_NAME_STATIC) == 1);
    let dm_id = first_message_id(&server.ctx).expect("DM id");
    eprintln!("[t1] after DM:       messages={}  reads={}  members={}",
        store_count(&server.ctx, Message::ENTITY_NAME_STATIC),
        store_count(&server.ctx, MessageRead::ENTITY_NAME_STATIC),
        store_count(&server.ctx, RoomMember::ENTITY_NAME_STATIC));

    // 2. B acks the DM (WS path → resolves to B via client_id).
    let ack = client_b.send_command::<AckMessages, AckMessagesResult>(&AckMessages {
        message_ids: vec![MessageId(dm_id.clone())],
        as_session: None,
    });
    {
        let cell = ack.clone();
        wait_for("B's ack response", move || matches!(cell.get(), Some(Ok(_))));
    }
    let ack_res = ack.get().expect("ack resp").expect("ok");
    wait_for("ack persisted", || {
        store_count(&server.ctx, MessageRead::ENTITY_NAME_STATIC) >= 1
    });
    eprintln!("[t2] after ack:      messages={}  reads={}  newly_acked={}",
        store_count(&server.ctx, Message::ENTITY_NAME_STATIC),
        store_count(&server.ctx, MessageRead::ENTITY_NAME_STATIC),
        ack_res.newly_acked);

    // 3. B DELs its session (shim deregister on exit). Local-origin → cascade.
    send_session_del(&client_b, &s);
    wait_for("B session gone (server)", || {
        !session_in_server(&server.ctx, "sess-bravo")
    });
    eprintln!("[t3] after DEL S:    messages={}  reads={}  members={}",
        store_count(&server.ctx, Message::ENTITY_NAME_STATIC),
        store_count(&server.ctx, MessageRead::ENTITY_NAME_STATIC),
        store_count(&server.ctx, RoomMember::ENTITY_NAME_STATIC));

    // 4. B re-registers the SAME session id.
    send_session_set(&client_b, &s);
    wait_for("B session re-registered (server)", || {
        session_in_server(&server.ctx, "sess-bravo")
    });
    eprintln!("[t4] after re-SET S: messages={}  reads={}  members={}",
        store_count(&server.ctx, Message::ENTITY_NAME_STATIC),
        store_count(&server.ctx, MessageRead::ENTITY_NAME_STATIC),
        store_count(&server.ctx, RoomMember::ENTITY_NAME_STATIC));

    // 5. B reads its unread inbox (what marshal-pi injects per prompt).
    let read = client_b.send_command::<ReadMessages, ReadMessagesResult>(&ReadMessages {
        room: None,
        from: None,
        to_session: None,
        inbox: true,
        sent: false,
        unread: true,
        since: None,
        limit: Some(20),
        as_session: None,
    });
    {
        let cell = read.clone();
        wait_for("B's read response", move || matches!(cell.get(), Some(Ok(_))));
    }
    let res = read.get().expect("read resp").expect("ok");
    eprintln!("[t5] read unread:    matched={}  returned={}",
        res.total_matched, res.messages.len());
    for m in &res.messages {
        eprintln!("    → {} from {} read_by_me={} body={:?}",
            m.message_id.0.as_ref(), m.from_session_id.0.as_ref(),
            m.read_by_me, m.body);
    }

    // Regression guard for the "re-acked inbox" report: a session's id is stable
    // across a pi exit / `/reload`, so read-state must be STICKY. An already-read
    // direct DM must not re-appear as unread after the session DEL + re-SET. The
    // ack follows the message (belongs_to Message) but NOT the session, so a
    // temporary disconnect doesn't re-unack the backlog.
    let re_injected = res
        .messages
        .iter()
        .any(|m| m.message_id.0.as_ref() == dm_id.as_ref());
    eprintln!("[verdict] direct DM re-injected as unread after DEL+re-SET: {re_injected}");
    assert!(
        !re_injected,
        "sticky-ack invariant violated: an already-read DM re-appeared as unread after the session DEL + re-registration"
    );

    server.shutdown();
}
