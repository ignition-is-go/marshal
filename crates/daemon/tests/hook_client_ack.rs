//! A prompt hook that asks to ack its own mail (`ack=client`) gets
//! the ids in `X-Marshal-Ack` and the mail stays unread until it acks: a reply
//! lost after the daemon flushed it doesn't lose the mail. A caller that
//! doesn't ask keeps the daemon's own ack.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use marshal_entities::{SendMessage, Session, SessionId};
use myko::{
    command::{CommandContext, CommandHandler},
    entities::client::ClientId,
    request::RequestContext,
    server::{MykoServerContext, Persister},
    wire::{MEvent, MEventType},
};
use myko_server::{BlackholePersister, MykoServer};
use uuid::Uuid;

const RECIPIENT: &str = r#"{"session_id":"recipient","cwd":"/work/repo"}"#;

fn pick_free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("pick free port");
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn sender() -> Session {
    Session {
        id: SessionId(Arc::from("sender")),
        client_id: Some(ClientId(Arc::from("c-sender"))),
        pid: 0,
        cwd: "/repo".into(),
        git_branch: None,
        current_task: None,
        session_name: None,
        activity: None,
        kind: None,
        connected_at: 100,
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
        operator: None,
        host: None,
        project: None,
        channels_enabled: None,
    }
}

fn cmd_ctx(ctx: &MykoServerContext) -> CommandContext {
    let req = RequestContext::new(
        Arc::<str>::from(Uuid::new_v4().to_string().as_str()),
        Some(Arc::<str>::from("c-sender")),
        vec![Arc::<str>::from("test")],
        Uuid::new_v4(),
        chrono::Utc::now().to_rfc3339(),
    );
    CommandContext::new(
        Arc::<str>::from("SendMessage"),
        Arc::new(req),
        Arc::new(ctx.clone()),
    )
}

fn send(ctx: &MykoServerContext, body: &str) {
    SendMessage {
        response_required: false,
        to_session_id: SessionId(Arc::from("recipient")),
        body: body.into(),
        as_session: None,
    }
    .execute(cmd_ctx(ctx))
    .expect("send to recipient");
}

fn request(target: &str, body: &str) -> String {
    format!(
        "POST {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn http_post(addr: SocketAddr, target: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect hook listener");
    s.write_all(request(target, body).as_bytes())
        .expect("write request");
    let mut resp = String::new();
    s.read_to_string(&mut resp).expect("read response");
    resp.split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or("")
        .to_string()
}

/// The whole response, status line and headers included.
fn http_post_raw(addr: SocketAddr, target: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect hook listener");
    s.write_all(request(target, body).as_bytes())
        .expect("write request");
    let mut resp = String::new();
    s.read_to_string(&mut resp).expect("read response");
    resp
}

/// Bring up a daemon context with the real hook listener and a seeded sender.
fn start() -> (MykoServerContext, SocketAddr) {
    let (ctx, addr, stop) = start_stoppable();
    // Never stopped: leak the sender so the listener keeps running.
    Box::leak(Box::new(stop));
    (ctx, addr)
}

fn start_stoppable() -> (
    MykoServerContext,
    SocketAddr,
    tokio::sync::watch::Sender<bool>,
) {
    marshal_entities::link();
    daemon::link();
    let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
    let server = MykoServer::builder()
        .with_default_persister(blackhole)
        .build();
    let ctx = server.ctx();
    Box::leak(Box::new(server));
    let event = MEvent::from_item(&sender(), MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event]).expect("seed sender");

    let addr: SocketAddr = format!("127.0.0.1:{}", pick_free_port()).parse().unwrap();
    let listener_ctx = ctx.clone();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async move {
            let _ = daemon::http_listener::run_until(addr, Arc::new(listener_ctx), stop_rx).await;
        });
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline, "hook listener never came up");
        thread::sleep(Duration::from_millis(25));
    }
    // Register the recipient so sends to it are routable.
    http_post(addr, "/hook/session-register?harness=claude", RECIPIENT);
    (ctx, addr, stop)
}

fn ack_ids(raw: &str) -> Vec<String> {
    raw.lines()
        .find_map(|l| l.strip_prefix("X-Marshal-Ack: "))
        .map(|ids| ids.trim().split(',').map(str::to_string).collect())
        .unwrap_or_default()
}

#[test]
fn a_prompt_hook_that_acks_itself_keeps_unacked_mail_unread() {
    // A short lease, set before the listener starts, so the test needn't wait a minute.
    unsafe { std::env::set_var("MARSHAL_ACK_LEASE_SECS", "1") };
    let (ctx, addr) = start();

    // 1. Offered with its id, and not marked read.
    send(&ctx, "CLIENT-ACK-lost-reply");
    let raw = http_post_raw(
        addr,
        "/hook/prompt-submit?harness=claude&ack=client",
        RECIPIENT,
    );
    assert!(raw.contains("CLIENT-ACK-lost-reply"), "{raw}");
    let ids = ack_ids(&raw);
    assert_eq!(ids.len(), 1, "the reply names the id to ack: {raw}");

    // 2. The reply is lost: no ack comes. Within the lease it isn't offered twice.
    let leased = http_post(
        addr,
        "/hook/prompt-submit?harness=claude&ack=client",
        RECIPIENT,
    );
    assert!(!leased.contains("CLIENT-ACK-lost-reply"), "{leased}");

    // 3. Once the lease lapses it's offered again.
    thread::sleep(Duration::from_millis(1500));
    let raw = http_post_raw(
        addr,
        "/hook/prompt-submit?harness=claude&ack=client",
        RECIPIENT,
    );
    assert!(
        raw.contains("CLIENT-ACK-lost-reply"),
        "unacked mail must come again: {raw}"
    );

    // 4. Acked this time: gone for good, lease or no lease.
    let ack = serde_json::json!({ "session_id": "recipient", "message_ids": ack_ids(&raw) });
    http_post(addr, "/hook/ack", &ack.to_string());
    thread::sleep(Duration::from_millis(1500));
    let after = http_post(
        addr,
        "/hook/prompt-submit?harness=claude&ack=client",
        RECIPIENT,
    );
    assert!(!after.contains("CLIENT-ACK-lost-reply"), "{after}");

    // 5. A caller that doesn't ask keeps the daemon's ack, as before.
    send(&ctx, "SERVER-ACK-old-caller");
    let raw = http_post_raw(addr, "/hook/prompt-submit?harness=claude", RECIPIENT);
    assert!(
        raw.contains("SERVER-ACK-old-caller") && ack_ids(&raw).is_empty(),
        "{raw}"
    );
    thread::sleep(Duration::from_millis(1500));
    let again = http_post(addr, "/hook/prompt-submit?harness=claude", RECIPIENT);
    assert!(!again.contains("SERVER-ACK-old-caller"), "{again}");
}
