use daemon::conn::AppState;
use daemon::db::Store;
use daemon::state::Roster;
use proto::framing::{read_frame, write_frame};
use proto::messages::{ClientMsg, ServerMsg};
use proto::rpc::{method, OkResult, RosterResult, SetStatusParams};
use std::sync::Arc;
use std::path::PathBuf;
use tokio::net::UnixStream;

async fn boot() -> (Arc<AppState>, UnixStream) {
    let app = Arc::new(AppState {
        roster: Roster::new(),
        store: tokio::sync::Mutex::new(Store::open_in_memory().unwrap()),
    });
    let (a, b) = UnixStream::pair().unwrap();
    let handler = Arc::clone(&app);
    tokio::spawn(async move {
        daemon::conn::handle(handler, a).await.ok();
    });
    (app, b)
}

#[tokio::test]
async fn hello_gets_welcome_with_cwd_nickname() {
    let (_app, mut sock) = boot().await;
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
    let (_app, mut sock) = boot().await;
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
    let (_app, mut sock) = boot().await;
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
