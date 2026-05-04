use crate::db::Store;
use crate::state::Roster;
use anyhow::{anyhow, Context, Result};
use proto::framing::{read_frame, write_frame};
use proto::messages::{ClientMsg, ServerMsg, SessionInfo};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

pub struct AppState {
    pub roster: Roster,
    pub store: tokio::sync::Mutex<Store>,
    /// Per-session sender for outbound frames (welcome, RPC replies, events).
    /// Used by RPC handlers to push asynchronous events (e.g. new_message) to
    /// other live sessions without blocking the sending session's reply path.
    pub outbound: std::sync::Mutex<HashMap<String, mpsc::UnboundedSender<ServerMsg>>>,
}

impl AppState {
    pub fn new(roster: Roster, store: Store) -> Self {
        Self {
            roster,
            store: tokio::sync::Mutex::new(store),
            outbound: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Push an event to the given session's outbound channel. Returns false if
    /// the session is not registered (disconnected) or its channel is closed.
    pub fn push_event(&self, to_session: &str, event: ServerMsg) -> bool {
        let g = self.outbound.lock().unwrap();
        match g.get(to_session) {
            Some(tx) => tx.send(event).is_ok(),
            None => false,
        }
    }
}

pub async fn handle(app: Arc<AppState>, sock: UnixStream) -> Result<()> {
    let (read_half, write_half) = sock.into_split();
    let mut read_half = read_half;

    // Read Hello.
    let frame = read_frame(&mut read_half).await.context("reading hello frame")?;
    let hello: ClientMsg = serde_json::from_slice(&frame).context("decoding hello")?;
    let (pid, cwd, git_branch) = match hello {
        ClientMsg::Hello { pid, cwd, git_branch } => (pid, cwd, git_branch),
        _ => return Err(anyhow!("expected Hello as first frame")),
    };

    let nickname = nickname_from_cwd(&cwd);
    let now = now_ms();
    let session_id = {
        let mut id = new_session_id();
        let mut info = SessionInfo {
            session_id: id.clone(),
            nickname: nickname.clone(),
            pid,
            cwd: cwd.clone(),
            git_branch: git_branch.clone(),
            current_task: None,
            connected_at: now,
            last_heartbeat: now,
            is_self: false,
        };
        let mut attempts = 0;
        loop {
            if app.roster.try_insert(info.clone()) { break id; }
            attempts += 1;
            if attempts >= 5 {
                return Err(anyhow!("could not allocate unique session_id after 5 attempts"));
            }
            id = new_session_id();
            info.session_id = id.clone();
        }
    };

    // Outbound channel: dispatch loop and other sessions push frames here;
    // the writer task drains them onto the socket.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();
    {
        let mut g = app.outbound.lock().unwrap();
        g.insert(session_id.clone(), out_tx.clone());
    }

    // Send Welcome through the same channel so ordering is preserved.
    let _ = out_tx.send(ServerMsg::Welcome {
        session_id: session_id.clone(),
        nickname: nickname.clone(),
    });

    // Snapshot the roster (excluding self) and push a `joined` event with the
    // current peers so the new session can announce itself to the user.
    let peers: Vec<SessionInfo> = app
        .roster
        .snapshot(&session_id)
        .into_iter()
        .filter(|s| s.session_id != session_id)
        .collect();
    let _ = out_tx.send(ServerMsg::Event {
        kind: "joined".to_string(),
        payload: serde_json::json!({
            "session_id": session_id,
            "nickname": nickname,
            "peers": peers,
        }),
    });

    // Broadcast `agent_joined` to every other live session so they can announce
    // the newcomer to their users.
    {
        let g = app.outbound.lock().unwrap();
        let event = ServerMsg::Event {
            kind: "agent_joined".to_string(),
            payload: serde_json::json!({
                "session_id": session_id,
                "nickname": nickname,
                "pid": pid,
                "cwd": cwd,
                "git_branch": git_branch,
                "connected_at": now,
            }),
        };
        for (sid, tx) in g.iter() {
            if sid != &session_id {
                let _ = tx.send(event.clone());
            }
        }
    }

    // Spawn writer task that owns the write half of the socket.
    let writer = tokio::spawn(async move {
        let mut write_half = write_half;
        while let Some(msg) = out_rx.recv().await {
            let buf = match serde_json::to_vec(&msg) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if write_frame(&mut write_half, &buf).await.is_err() {
                break;
            }
        }
    });

    // Dispatch loop reads RPCs and pushes replies via out_tx.
    let result = dispatch_loop(&app, &session_id, &mut read_half, &out_tx).await;

    // Cleanup: drop the outbound entry and our local sender so the writer task exits.
    {
        let mut g = app.outbound.lock().unwrap();
        g.remove(&session_id);
    }
    drop(out_tx);
    let _ = writer.await;

    app.roster.remove(&session_id);
    result
}

async fn dispatch_loop(
    app: &Arc<AppState>,
    session_id: &str,
    read_half: &mut tokio::net::unix::OwnedReadHalf,
    out_tx: &mpsc::UnboundedSender<ServerMsg>,
) -> Result<()> {
    loop {
        let frame = match read_frame(read_half).await {
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
                let _ = out_tx.send(err);
            }
            ClientMsg::Rpc { id, method, params } => {
                app.roster.touch_heartbeat(session_id, now_ms());
                let resp = crate::rpc::dispatch(app, session_id, id, &method, params).await;
                let _ = out_tx.send(resp);
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
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).expect("getrandom failed");
    format!("s-{:08x}", u32::from_le_bytes(bytes))
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
