use daemon::conn::AppState;
use daemon::db::Store;
use daemon::state::Roster;
use proto::framing::{read_frame, write_frame};
use proto::messages::{ClientMsg, ServerMsg};
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
