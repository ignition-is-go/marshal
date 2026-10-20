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

use marshal_entities::{
    GetAllMessageReads, GetAllMessages, GetAllSessions, Message, MessageRead, SendMessage, Session,
    SessionId,
};
use myko::{
    command::{CommandContext, CommandHandler},
    entities::client::ClientId,
    request::RequestContext,
    server::{MykoServerContext, Persister},
    wire::{MEvent, MEventType},
};
use myko_server::{BlackholePersister, MykoServer};
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

fn set_session(ctx: &MykoServerContext, s: &Session) {
    let event = MEvent::from_item(s, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event])
        .expect("apply Session SET");
}

fn cmd_ctx(ctx: &MykoServerContext, caller_client_id: Option<&str>) -> CommandContext {
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
    let server = MykoServer::builder()
        .with_default_persister(blackhole)
        .build();
    let ctx = server.ctx();
    let server: &'static MykoServer = Box::leak(Box::new(server));
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
    // its response identifies the reserved session without surfacing inbox.
    let registration = http_post(
        addr,
        "/hook/session-register?host=test-host&operator=test-op&harness=codex",
        r#"{"session_id":"recipient","cwd":"/work/repo"}"#,
    );
    let registration: serde_json::Value =
        serde_json::from_str(&registration).expect("registration identity JSON");
    assert_eq!(registration["session_id"], "recipient");
    assert!(
        registration["nickname"]
            .as_str()
            .is_some_and(|n| !n.is_empty())
    );

    // Reproduce a still-running Codex TUI whose hook-owned row was removed by
    // an older daemon's idle-session backstop. Its next prompt must recreate
    // the same canonical row in place, without restarting the TUI or changing
    // its session id.
    let deleted = MEvent::from_item(
        &session("recipient", None),
        MEventType::DEL,
        &Uuid::new_v4().to_string(),
    );
    ctx.apply_event_batch(vec![deleted])
        .expect("delete recipient session");
    let repaired = http_post(
        addr,
        "/hook/prompt-submit?host=test-host&operator=test-op&harness=codex",
        r#"{"session_id":"recipient","cwd":"/work/repo"}"#,
    );
    assert_eq!(repaired, "", "repair without inbox must inject no context");
    let sessions: Vec<Arc<Session>> = cmd_ctx(&ctx, Some("c-sender"))
        .exec_query(GetAllSessions {})
        .expect("query repaired sessions");
    let recipient = sessions
        .iter()
        .find(|session| session.id.0.as_ref() == "recipient")
        .expect("prompt-submit must recreate a missing session row");
    assert_eq!(recipient.cwd, "/work/repo");
    assert_eq!(recipient.operator.as_deref(), Some("test-op"));
    assert_eq!(
        recipient.host.as_ref().map(|host| host.name.as_str()),
        Some("test-host")
    );

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
    let refresh: serde_json::Value = serde_json::from_str(&refresh).expect("refresh identity JSON");
    assert_eq!(
        refresh, registration,
        "registration must keep its reserved identity"
    );

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

    let stored: Vec<Arc<Message>> = cmd_ctx(&ctx, Some("c-sender"))
        .exec_query(GetAllMessages {})
        .expect("query acknowledged message");
    let acknowledged = stored
        .iter()
        .find(|message| message.body == "HOOK-E2E-PROBE-marker")
        .expect("acknowledged Message must remain stored")
        .clone();
    let reads: Vec<Arc<MessageRead>> = cmd_ctx(&ctx, Some("c-sender"))
        .exec_query(GetAllMessageReads {})
        .expect("query message reads");
    assert!(
        reads.iter().any(|read| {
            read.message_id == acknowledged.id
                && read.session_id == SessionId(Arc::from("recipient"))
        }),
        "successful hook write must persist its recipient read receipt"
    );

    SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: "HOOK-E2E-QUEUED-marker".into(),
        as_session: None,
    }
    .execute(cmd_ctx(&ctx, Some("c-sender")))
    .expect("send persists unread message before session end");

    // Codex resumes a thread under the same session id after the terminal has
    // emitted SessionEnd. Messages and read receipts outlive that ephemeral
    // presence row, so ending the session must neither lose unread inbox nor
    // turn acknowledged history back into unread inbox.
    let ended = http_post(addr, "/hook/session-end", r#"{"session_id":"recipient"}"#);
    assert_eq!(ended, "");
    let after_end: Vec<Arc<Message>> = cmd_ctx(&ctx, Some("c-sender"))
        .exec_query(GetAllMessages {})
        .expect("query messages after session end");
    assert!(
        after_end
            .iter()
            .any(|message| message.id == acknowledged.id),
        "session end must retain acknowledged direct messages"
    );
    assert!(
        after_end
            .iter()
            .any(|message| message.body == "HOOK-E2E-QUEUED-marker"),
        "session end must retain unread direct messages"
    );
    let reads_after_end: Vec<Arc<MessageRead>> = cmd_ctx(&ctx, Some("c-sender"))
        .exec_query(GetAllMessageReads {})
        .expect("query message reads after session end");
    assert!(
        reads_after_end
            .iter()
            .any(|read| read.message_id == acknowledged.id),
        "session end must retain read receipts for durable messages"
    );
    let resumed = http_post(
        addr,
        "/hook/session-register?host=test-host&operator=test-op&harness=codex",
        r#"{"session_id":"recipient","cwd":"/work/repo"}"#,
    );
    let resumed: serde_json::Value =
        serde_json::from_str(&resumed).expect("resumed registration identity JSON");
    assert_eq!(resumed["session_id"], "recipient");
    let after_resume = http_post(
        addr,
        "/hook/prompt-submit",
        r#"{"session_id":"recipient","cwd":"/work/repo"}"#,
    );
    assert!(
        !after_resume.contains("HOOK-E2E-PROBE-marker"),
        "acknowledged message replayed after same-id session resume; got: {after_resume:?}"
    );
    assert!(
        after_resume.contains("HOOK-E2E-QUEUED-marker"),
        "unread message queued before session end did not surface after resume; got: {after_resume:?}"
    );

    SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: "HOOK-E2E-NEW-marker".into(),
        as_session: None,
    }
    .execute(cmd_ctx(&ctx, Some("c-sender")))
    .expect("send persists new message after resume");
    let new_after_resume = http_post(
        addr,
        "/hook/prompt-submit",
        r#"{"session_id":"recipient","cwd":"/work/repo"}"#,
    );
    assert!(
        new_after_resume.contains("HOOK-E2E-NEW-marker"),
        "new unread message did not surface after resume; got: {new_after_resume:?}"
    );

    let deleted = MEvent::from_item(
        acknowledged.as_ref(),
        MEventType::DEL,
        &Uuid::new_v4().to_string(),
    );
    ctx.apply_event_batch(vec![deleted])
        .expect("delete acknowledged Message");
    let reads: Vec<Arc<MessageRead>> = cmd_ctx(&ctx, Some("c-sender"))
        .exec_query(GetAllMessageReads {})
        .expect("query message reads after Message deletion");
    assert!(
        reads.iter().all(|read| read.message_id != acknowledged.id),
        "deleting a Message must cascade-delete its read receipts"
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
