use crate::db::Store;
use crate::state::Roster;
use anyhow::{anyhow, Context, Result};
use proto::framing::{read_frame, write_frame};
use proto::messages::{ClientMsg, ServerMsg, SessionInfo};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::UnixStream;

pub struct AppState {
    pub roster: Roster,
    pub store: tokio::sync::Mutex<Store>,
}

pub async fn handle(app: Arc<AppState>, mut sock: UnixStream) -> Result<()> {
    // Read Hello.
    let frame = read_frame(&mut sock).await.context("reading hello frame")?;
    let hello: ClientMsg = serde_json::from_slice(&frame).context("decoding hello")?;
    let (pid, cwd, git_branch) = match hello {
        ClientMsg::Hello { pid, cwd, git_branch } => (pid, cwd, git_branch),
        _ => return Err(anyhow!("expected Hello as first frame")),
    };

    let session_id = new_session_id();
    let nickname = nickname_from_cwd(&cwd);
    let now = now_ms();
    app.roster.insert(SessionInfo {
        session_id: session_id.clone(),
        nickname: nickname.clone(),
        pid,
        cwd,
        git_branch,
        current_task: None,
        connected_at: now,
        last_heartbeat: now,
        is_self: false,
    });

    let welcome = ServerMsg::Welcome {
        session_id: session_id.clone(),
        nickname: nickname.clone(),
    };
    write_frame(&mut sock, serde_json::to_vec(&welcome)?.as_slice()).await?;

    // Dispatch loop. RPC handlers are added in Task 11.
    let result = dispatch_loop(&app, &session_id, &mut sock).await;

    app.roster.remove(&session_id);
    result
}

async fn dispatch_loop(
    app: &Arc<AppState>,
    session_id: &str,
    sock: &mut UnixStream,
) -> Result<()> {
    loop {
        let frame = match read_frame(sock).await {
            Ok(f) => f,
            Err(proto::framing::FrameError::Io(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let msg: ClientMsg = serde_json::from_slice(&frame)?;
        match msg {
            ClientMsg::Hello { .. } => {
                let err = ServerMsg::RpcErr {
                    id: 0,
                    code: proto::messages::ErrorCode::BadRequest,
                    message: "duplicate hello".into(),
                };
                write_frame(sock, serde_json::to_vec(&err)?.as_slice()).await?;
            }
            ClientMsg::Rpc { id, method, params } => {
                app.roster.touch_heartbeat(session_id, now_ms());
                let resp = crate::rpc::dispatch(app, session_id, id, &method, params).await;
                write_frame(sock, serde_json::to_vec(&resp)?.as_slice()).await?;
            }
        }
    }
}

fn nickname_from_cwd(cwd: &std::path::Path) -> String {
    cwd.file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("session")
        .to_string()
}

fn new_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(0);
    let mut x = now_ms() as u64 ^ SEED.fetch_add(1, Ordering::Relaxed) ^ std::process::id() as u64;
    x ^= x << 13; x ^= x >> 7; x ^= x << 17;
    format!("s-{:04x}", (x as u16))
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
