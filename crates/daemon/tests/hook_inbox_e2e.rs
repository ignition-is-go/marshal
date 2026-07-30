//! End-to-end test of the `/hook/*` HTTP listener — registration plus the
//! flag-independent inbox-delivery path. Eager registration MUST create the
//! session without consuming context, and a stored message MUST subsequently
//! surface in the `<marshal_inbox>` block returned by
//! `POST /hook/prompt-submit`.
//!
//! This guards the daemon side of the path whose *deploy* misconfiguration
//! (hook listener on the wrong port / bound to loopback) silently broke
//! message receipt. The command-layer tests cover persistence; this one
//! drives the actual HTTP listener so a regression in `http_listener` /
//! `hooks::dispatch` / `surface_unread` fails CI.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use marshal_entities::{GetAllMessages, Message, SendMessage, Session, SessionId};
use myko::{
    command::{CommandContext, CommandHandler},
    entities::client::ClientId,
    request::RequestContext,
    server::{CellServerCtx, Persister},
    wire::{MEvent, MEventType},
};
use myko_server::{BlackholePersister, CellServer};
use uuid::Uuid;

fn pick_free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("pick free port");
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn session(id: &str, client_id: Option<&str>) -> Session {
    Session {
        id: SessionId(Arc::from(id)),
        client_id: client_id.map(|c| ClientId(Arc::from(c))),
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

fn set_session(ctx: &CellServerCtx, s: &Session) {
    let event = MEvent::from_item(s, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event])
        .expect("apply Session SET");
}

fn cmd_ctx(ctx: &CellServerCtx, caller_client_id: Option<&str>) -> CommandContext {
    let req = RequestContext::new(
        Arc::<str>::from(Uuid::new_v4().to_string().as_str()),
        caller_client_id.map(Arc::<str>::from),
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

/// Raw HTTP/1.1 POST (the listener replies `Connection: close`); returns the
/// response body.
fn http_post(addr: SocketAddr, target: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect hook listener");
    let req = format!(
        "POST {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).expect("write request");
    let mut resp = String::new();
    s.read_to_string(&mut resp).expect("read response");
    resp.split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or("")
        .to_string()
}

fn wait_listening(addr: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("hook listener never came up on {addr}");
}

#[test]
fn stored_message_surfaces_via_prompt_submit_hook() {
    marshal_entities::link();
    daemon::link();

    let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
    let server = CellServer::builder()
        .with_default_persister(blackhole)
        .build();
    let ctx = server.ctx();
    let server: &'static CellServer = Box::leak(Box::new(server));
    let _ = server;

    // Seed the sender; the recipient is created through the real eager
    // registration route below.
    set_session(&ctx, &session("sender", Some("c-sender")));

    // bring up the real /hook/* HTTP listener on a free port, sharing ctx.
    let port = pick_free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let listener_ctx = ctx.clone();
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async move {
            let _ = daemon::http_listener::run(addr, Arc::new(listener_ctx)).await;
        });
    });
    wait_listening(addr);

    // App-server lifecycle discovery creates the hook-owned session before
    // any user prompt. Registration has no model turn to receive context, so
    // its response must stay empty.
    let registration = http_post(
        addr,
        "/hook/session-register?host=test-host&operator=test-op&harness=codex",
        r#"{"session_id":"recipient","cwd":"/work/repo"}"#,
    );
    assert_eq!(registration, "", "registration must not inject context");

    // The successful send proves the registration route created a routable
    // Session row. Register it again after the message is stored to prove a
    // refresh cannot surface or acknowledge that unread message.
    SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: "HOOK-E2E-PROBE-marker".into(),
        as_session: None,
    }
    .execute(cmd_ctx(&ctx, Some("c-sender")))
    .expect("send persists to eagerly registered recipient");
    let refresh = http_post(
        addr,
        "/hook/session-register?host=test-host&operator=test-op&harness=codex",
        r#"{"session_id":"recipient","cwd":"/work/repo"}"#,
    );
    assert_eq!(refresh, "", "registration refresh must not inject context");

    // the recipient's prompt-submit hook must surface the stored message.
    let body = http_post(addr, "/hook/prompt-submit", r#"{"session_id":"recipient"}"#);
    assert!(
        body.contains("HOOK-E2E-PROBE-marker"),
        "message did not surface via /hook/prompt-submit; got: {body:?}"
    );
    assert!(
        body.contains("<marshal_inbox"),
        "expected a <marshal_inbox> block; got: {body:?}"
    );

    // At-least-once contract: the listener acks the surfaced message only
    // AFTER successfully writing the response (not inside dispatch). Since the
    // first POST above completed its write, the message is now read — a second
    // prompt-submit must NOT re-surface it. (A dropped write would skip the
    // ack and this would still contain the marker — that's the at-least-once
    // guarantee, verified here by its positive consequence.)
    let body2 = http_post(addr, "/hook/prompt-submit", r#"{"session_id":"recipient"}"#);
    assert!(
        !body2.contains("HOOK-E2E-PROBE-marker"),
        "message re-surfaced after a successful hook write — the post-write ack didn't run; got: {body2:?}"
    );

    // Automatic delivery carries a bounded, UTF-8-safe preview. The complete
    // durable Message remains queryable even after the preview is acknowledged.
    let long_body = format!("LONG-{}-TAIL", "é".repeat(3_000));
    SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: long_body.clone(),
        as_session: None,
    }
    .execute(cmd_ctx(&ctx, Some("c-sender")))
    .expect("send persists long message");
    let stored: Vec<Arc<Message>> = cmd_ctx(&ctx, Some("c-sender"))
        .exec_query(GetAllMessages {})
        .expect("query durable messages");
    assert!(
        stored.iter().any(|message| message.body == long_body),
        "durable Message must retain the complete body"
    );

    let bounded = http_post(addr, "/hook/prompt-submit", r#"{"session_id":"recipient"}"#);
    assert!(bounded.contains("LONG-"));
    assert!(!bounded.contains("-TAIL"), "hook leaked the complete body");
    assert!(
        bounded.contains("[truncated; full message"),
        "hook did not identify the bounded preview: {bounded:?}"
    );
    assert!(
        bounded.chars().count() < 3_000,
        "automatic inbox context exceeded its per-message bound"
    );
    let after_bounded = http_post(addr, "/hook/prompt-submit", r#"{"session_id":"recipient"}"#);
    assert!(
        !after_bounded.contains("LONG-"),
        "bounded preview re-surfaced after successful hook write"
    );

    // a wrong /hook path must 404 (the misroute that caused the outage).
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(
        b"POST /hook/does-not-exist HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(
        raw.starts_with("HTTP/1.1 404"),
        "unknown hook path must 404; got: {raw:?}"
    );
}
