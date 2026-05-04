use daemon::conn::AppState;
use daemon::db::Store;
use daemon::state::Roster;
use proto::framing::{read_frame, write_frame};
use proto::messages::{ClientMsg, ServerMsg};
use proto::rpc::{method, OkResult, RosterResult, SendMessageParams, SendMessageResult, SetStatusParams};
use proto::rpc::{InboxParams, InboxResult, RecentMessagesParams, RecentMessagesResult};
use std::sync::Arc;
use std::path::PathBuf;
use tokio::net::UnixStream;

struct Harness {
    app: Arc<AppState>,
}
impl Harness {
    async fn new() -> Self {
        let app = Arc::new(AppState {
            roster: Roster::new(),
            store: tokio::sync::Mutex::new(Store::open_in_memory().unwrap()),
        });
        Self { app }
    }
    async fn connect(&self) -> UnixStream {
        let (a, b) = UnixStream::pair().unwrap();
        let app = Arc::clone(&self.app);
        tokio::spawn(async move { daemon::conn::handle(app, a).await.ok(); });
        b
    }
}

#[tokio::test]
async fn hello_gets_welcome_with_cwd_nickname() {
    let h = Harness::new().await;
    let mut sock = h.connect().await;
    let hello = ClientMsg::Hello {
        pid: 99,
        cwd: PathBuf::from("/home/trevor/Code/eww"),
        git_branch: Some("main".into()),
    };
    write_frame(&mut sock, serde_json::to_vec(&hello).unwrap().as_slice()).await.unwrap();
    let frame = read_frame(&mut sock).await.unwrap();
    let resp: ServerMsg = serde_json::from_slice(&frame).unwrap();
    match resp {
        ServerMsg::Welcome { session_id, nickname } => {
            assert!(session_id.starts_with("s-"));
            assert_eq!(nickname, "eww");
        }
        other => panic!("expected Welcome, got {other:?}"),
    }
}

async fn say_hello(sock: &mut UnixStream, cwd: &str) -> String {
    let hello = ClientMsg::Hello { pid: 1, cwd: cwd.into(), git_branch: None };
    write_frame(sock, serde_json::to_vec(&hello).unwrap().as_slice()).await.unwrap();
    let frame = read_frame(sock).await.unwrap();
    match serde_json::from_slice(&frame).unwrap() {
        ServerMsg::Welcome { session_id, .. } => session_id,
        m => panic!("expected welcome, got {m:?}"),
    }
}

async fn rpc(sock: &mut UnixStream, id: u64, method: &str, params: serde_json::Value)
    -> ServerMsg
{
    let req = ClientMsg::Rpc { id, method: method.into(), params };
    write_frame(sock, serde_json::to_vec(&req).unwrap().as_slice()).await.unwrap();
    let frame = read_frame(sock).await.unwrap();
    serde_json::from_slice(&frame).unwrap()
}

#[tokio::test]
async fn roster_lists_self() {
    let h = Harness::new().await;
    let mut sock = h.connect().await;
    let me = say_hello(&mut sock, "/x/eww").await;
    let resp = rpc(&mut sock, 1, method::ROSTER, serde_json::json!({})).await;
    let result = match resp {
        ServerMsg::RpcOk { result, .. } => result,
        other => panic!("expected ok, got {other:?}"),
    };
    let r: RosterResult = serde_json::from_value(result).unwrap();
    assert_eq!(r.sessions.len(), 1);
    assert_eq!(r.sessions[0].session_id, me);
    assert!(r.sessions[0].is_self);
    assert_eq!(r.sessions[0].nickname, "eww");
}

#[tokio::test]
async fn set_status_updates_roster() {
    let h = Harness::new().await;
    let mut sock = h.connect().await;
    let _me = say_hello(&mut sock, "/x/eww").await;
    let p = SetStatusParams { text: "refactoring".into() };
    let resp = rpc(&mut sock, 1, method::SET_STATUS, serde_json::to_value(&p).unwrap()).await;
    let _: OkResult = match resp {
        ServerMsg::RpcOk { result, .. } => serde_json::from_value(result).unwrap(),
        other => panic!("expected ok, got {other:?}"),
    };
    let r2 = rpc(&mut sock, 2, method::ROSTER, serde_json::json!({})).await;
    let result = match r2 { ServerMsg::RpcOk { result, .. } => result, _ => panic!() };
    let r: RosterResult = serde_json::from_value(result).unwrap();
    assert_eq!(r.sessions[0].current_task.as_deref(), Some("refactoring"));
}

#[tokio::test]
async fn send_message_to_known_nickname_succeeds() {
    let h = Harness::new().await;
    let mut sender = h.connect().await;
    let mut recv = h.connect().await;
    let _ = say_hello(&mut sender, "/x/sender").await;
    let _ = say_hello(&mut recv, "/x/eww").await;

    let p = SendMessageParams { to: "eww".into(), body: "hi".into() };
    let resp = rpc(&mut sender, 1, method::SEND_MESSAGE, serde_json::to_value(&p).unwrap()).await;
    let r: SendMessageResult = match resp {
        ServerMsg::RpcOk { result, .. } => serde_json::from_value(result).unwrap(),
        other => panic!("{other:?}"),
    };
    assert!(r.message_id > 0);
}

#[tokio::test]
async fn send_message_unknown_recipient_errors() {
    let h = Harness::new().await;
    let mut sender = h.connect().await;
    let _ = say_hello(&mut sender, "/x/sender").await;
    let p = SendMessageParams { to: "ghost".into(), body: "x".into() };
    let resp = rpc(&mut sender, 1, method::SEND_MESSAGE, serde_json::to_value(&p).unwrap()).await;
    match resp {
        ServerMsg::RpcErr { code, .. } => assert_eq!(code, proto::messages::ErrorCode::UnknownRecipient),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn send_message_ambiguous_recipient_errors() {
    let h = Harness::new().await;
    let mut a = h.connect().await;
    let mut b1 = h.connect().await;
    let mut b2 = h.connect().await;
    let _ = say_hello(&mut a, "/x/sender").await;
    let _ = say_hello(&mut b1, "/y/eww").await;
    let _ = say_hello(&mut b2, "/z/eww").await;
    let p = SendMessageParams { to: "eww".into(), body: "x".into() };
    let resp = rpc(&mut a, 1, method::SEND_MESSAGE, serde_json::to_value(&p).unwrap()).await;
    match resp {
        ServerMsg::RpcErr { code, .. } =>
            assert_eq!(code, proto::messages::ErrorCode::AmbiguousRecipient),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn inbox_returns_unread_then_marks_read() {
    let h = Harness::new().await;
    let mut sender = h.connect().await;
    let mut recv = h.connect().await;
    let _ = say_hello(&mut sender, "/x/sender").await;
    let _ = say_hello(&mut recv, "/x/eww").await;

    let p = SendMessageParams { to: "eww".into(), body: "yo".into() };
    let _ = rpc(&mut sender, 1, method::SEND_MESSAGE, serde_json::to_value(&p).unwrap()).await;

    let inbox: InboxResult = match rpc(&mut recv, 1, method::INBOX,
        serde_json::to_value(InboxParams { mark_read: true }).unwrap()).await {
        ServerMsg::RpcOk { result, .. } => serde_json::from_value(result).unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].body, "yo");
    assert_eq!(inbox.messages[0].from_nick, "sender");

    // Second call returns nothing.
    let inbox2: InboxResult = match rpc(&mut recv, 2, method::INBOX,
        serde_json::to_value(InboxParams { mark_read: true }).unwrap()).await {
        ServerMsg::RpcOk { result, .. } => serde_json::from_value(result).unwrap(),
        other => panic!("{other:?}"),
    };
    assert!(inbox2.messages.is_empty());
}

#[tokio::test]
async fn recent_messages_includes_sent_and_received() {
    let h = Harness::new().await;
    let mut sender = h.connect().await;
    let mut recv = h.connect().await;
    let _ = say_hello(&mut sender, "/x/sender").await;
    let _ = say_hello(&mut recv, "/x/eww").await;

    rpc(&mut sender, 1, method::SEND_MESSAGE, serde_json::to_value(
        SendMessageParams { to: "eww".into(), body: "hi".into() }).unwrap()).await;
    rpc(&mut recv, 1, method::SEND_MESSAGE, serde_json::to_value(
        SendMessageParams { to: "sender".into(), body: "back".into() }).unwrap()).await;

    let rec: RecentMessagesResult = match rpc(&mut sender, 2, method::RECENT_MESSAGES,
        serde_json::to_value(RecentMessagesParams { limit: 50 }).unwrap()).await {
        ServerMsg::RpcOk { result, .. } => serde_json::from_value(result).unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(rec.messages.len(), 2);
    let dirs: Vec<_> = rec.messages.iter().map(|m| m.direction).collect();
    use proto::rpc::Direction;
    assert!(dirs.contains(&Direction::Sent));
    assert!(dirs.contains(&Direction::Received));
}
