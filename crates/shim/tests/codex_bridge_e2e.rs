//! Process-level Codex wake canaries.
//!
//! These tests deliberately launch the real `marshal-shim codex-bridge`
//! executable. A real in-process Myko cell supplies Session/Message views and
//! fake loopback app-servers speak Codex's WebSocket protocol. This catches
//! failures that an in-process lock unit test cannot:
//!
//! - Unix: two bridge processes sharing one app-server elect exactly one wake
//!   leader, then the follower takes over after the leader exits.
//! - Windows: two isolated app-server endpoints each wake only the thread they
//!   own, matching native `codex-run`'s per-TUI topology.

use std::{
    collections::HashSet,
    io,
    net::SocketAddr,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use marshal_entities::{
    GetAllSessions, HostInfo, Message, MessageId, MessageRead, MessageReadId, Session, SessionId,
};
use myko::prelude::EventPublishing as _;
use myko::{
    command::CommandContext,
    request::RequestContext,
    server::{MykoServerContext, Persister},
};
use myko_server::{BlackholePersister, MykoServer};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_tungstenite::{accept_async, tungstenite::Message as WebSocketMessage};

const TEST_HOST: &str = "marshal-wake-canary";
const POLL: Duration = Duration::from_millis(25);
const WAIT: Duration = Duration::from_secs(8);
const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");

struct CellHarness {
    address: String,
    ctx: Arc<MykoServerContext>,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl CellHarness {
    fn spawn() -> Self {
        marshal_entities::link();
        let bind: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();

        let join = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("build cell runtime");
            runtime.block_on(async move {
                let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
                let server = Arc::new(
                    MykoServer::builder()
                        .with_bind_addr(bind)
                        .with_default_persister(blackhole)
                        .build(),
                );
                ready_tx.send(server.ctx()).expect("publish cell context");
                tokio::select! {
                    result = server.run() => {
                        result.expect("run test cell");
                    }
                    _ = tokio::task::spawn_blocking(move || shutdown_rx.recv()) => {}
                }
            });
        });
        let ctx = Arc::new(
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("cell context"),
        );

        Self {
            address: format!("ws://{bind}"),
            ctx,
            shutdown: Some(shutdown_tx),
            join: Some(join),
        }
    }
}

impl Drop for CellHarness {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct HookHarness {
    base_url: String,
    task: JoinHandle<()>,
}

impl HookHarness {
    async fn spawn(ctx: Arc<MykoServerContext>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind hook listener");
        let address = listener.local_addr().expect("hook address");
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept hook request");
                let ctx = Arc::clone(&ctx);
                tokio::spawn(async move {
                    handle_hook_request(stream, ctx)
                        .await
                        .expect("handle hook request");
                });
            }
        });
        Self {
            base_url: format!("http://{address}"),
            task,
        }
    }
}

impl Drop for HookHarness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_hook_request(mut stream: TcpStream, ctx: Arc<MykoServerContext>) -> io::Result<()> {
    let (target, body) = read_http_request(&mut stream).await?;
    if target.starts_with("/hook/session-register")
        && let Ok(body) = serde_json::from_slice::<Value>(&body)
        && let Some(session_id) = body.get("session_id").and_then(Value::as_str)
    {
        let cwd = body
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("/wake-canary");
        internal_context(&ctx)
            .emit_set(&hook_session(session_id, cwd))
            .expect("register hook-owned session");
    }
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await?;
    stream.flush().await
}

async fn read_http_request(stream: &mut TcpStream) -> io::Result<(String, Vec<u8>)> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 2048];
    let (header_end, content_length, target) = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "hook request closed before headers",
            ));
        }
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let target = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        break (header_end, content_length, target);
    };
    let body_start = header_end + 4;
    while request.len() < body_start + content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
    }
    Ok((
        target,
        request
            .get(body_start..body_start + content_length)
            .unwrap_or_default()
            .to_vec(),
    ))
}

struct FakeAppServer {
    endpoint: String,
    accepted: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl FakeAppServer {
    async fn spawn(owned_threads: &[&str]) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake app-server");
        let address = listener.local_addr().expect("fake app-server address");
        let owned: Arc<HashSet<String>> = Arc::new(
            owned_threads
                .iter()
                .map(|thread| (*thread).to_string())
                .collect(),
        );
        let accepted = Arc::new(Mutex::new(Vec::new()));
        let accepted_for_task = Arc::clone(&accepted);
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept app-server client");
                let owned = Arc::clone(&owned);
                let accepted = Arc::clone(&accepted_for_task);
                tokio::spawn(async move {
                    // Tests terminate bridge processes abruptly; a reset
                    // without a WebSocket close handshake is expected cleanup,
                    // not a failed canary assertion.
                    let _ = serve_app_server_connection(stream, owned, accepted).await;
                });
            }
        });
        Self {
            endpoint: format!("ws://{address}"),
            accepted,
            task,
        }
    }

    fn accepted(&self) -> Vec<String> {
        self.accepted.lock().expect("accepted wake mutex").clone()
    }

    async fn wait_for_count(&self, count: usize) {
        wait_until("accepted wake count", WAIT, || {
            self.accepted.lock().expect("accepted wake mutex").len() >= count
        })
        .await;
    }
}

impl Drop for FakeAppServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_app_server_connection(
    stream: TcpStream,
    owned_threads: Arc<HashSet<String>>,
    accepted: Arc<Mutex<Vec<String>>>,
) -> anyhow::Result<()> {
    let mut websocket = accept_async(stream).await?;
    let initialize = websocket
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("connection closed before initialize"))??;
    let initialize: Value = serde_json::from_str(initialize.to_text()?)?;
    anyhow::ensure!(initialize["method"] == "initialize");
    let initialize_id = initialize.get("id").cloned().unwrap_or(json!(1));
    websocket
        .send(WebSocketMessage::Text(
            json!({
                "id": initialize_id,
                "result": {
                    "userAgent": "marshal-wake-canary",
                    "platformFamily": "test",
                    "platformOs": std::env::consts::OS,
                    "codexHome": "/tmp"
                }
            })
            .to_string()
            .into(),
        ))
        .await?;

    let initialized = websocket
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("connection closed before initialized"))??;
    let initialized: Value = serde_json::from_str(initialized.to_text()?)?;
    anyhow::ensure!(initialized["method"] == "initialized");

    // A real app-server broadcasts these when an attached TUI creates/resumes
    // a thread. Sending them on every initialized connection is harmless:
    // lifecycle bridges register them, while one-shot wake clients ignore
    // notifications until their matching RPC response arrives.
    for thread in owned_threads.iter() {
        websocket
            .send(WebSocketMessage::Text(
                json!({
                    "method": "thread/started",
                    "params": {
                        "thread": {
                            "id": thread,
                            "sessionId": thread,
                            "cwd": format!("/wake-canary/{thread}")
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await?;
    }

    while let Some(frame) = websocket.next().await {
        let frame = frame?;
        let WebSocketMessage::Text(text) = frame else {
            continue;
        };
        let request: Value = serde_json::from_str(text.as_ref())?;
        if request.get("method").and_then(Value::as_str) != Some("turn/start") {
            continue;
        }
        let id = request.get("id").cloned().unwrap_or(json!(2));
        let thread = request
            .pointer("/params/threadId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let response = if owned_threads.contains(thread) {
            accepted
                .lock()
                .expect("accepted wake mutex")
                .push(thread.to_string());
            json!({
                "id": id,
                "result": { "turn": { "id": format!("turn-{thread}") } }
            })
        } else {
            json!({
                "id": id,
                "error": { "code": -32602, "message": "thread is not owned by this endpoint" }
            })
        };
        websocket
            .send(WebSocketMessage::Text(response.to_string().into()))
            .await?;
    }
    Ok(())
}

struct BridgeProcess {
    child: Option<Child>,
    ready_file: std::path::PathBuf,
}

impl BridgeProcess {
    fn spawn(
        app_server: &FakeAppServer,
        cell: &CellHarness,
        hooks: &HookHarness,
        temp: &TempDir,
        name: &str,
    ) -> Self {
        let ready_file = temp.path().join(format!("{name}.ready"));
        let mut command = Command::new(SHIM);
        command
            .args([
                "codex-bridge",
                "--endpoint",
                &app_server.endpoint,
                "--daemon",
                &cell.address,
                "--poll-ms",
                &POLL.as_millis().to_string(),
                "--ready-file",
            ])
            .arg(&ready_file)
            .env("MARSHAL_BASE_URL", &hooks.base_url)
            .env("MARSHAL_HOST", TEST_HOST)
            .env("RUST_LOG", "marshal_shim=debug")
            .env("TMPDIR", temp.path())
            .env("TEMP", temp.path())
            .env("TMP", temp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        let child = command.spawn().expect("launch real codex-bridge process");
        Self {
            child: Some(child),
            ready_file,
        }
    }

    async fn wait_ready(&mut self) {
        let deadline = Instant::now() + WAIT;
        loop {
            if self.ready_file.is_file() {
                let _ = std::fs::remove_file(&self.ready_file);
                return;
            }
            if let Some(status) = self
                .child
                .as_mut()
                .expect("bridge child")
                .try_wait()
                .expect("check bridge child")
            {
                panic!("codex-bridge exited before readiness: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for codex-bridge readiness"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve port");
    listener.local_addr().expect("reserved address").port()
}

fn internal_context(ctx: &Arc<MykoServerContext>) -> CommandContext {
    let request = RequestContext::internal(
        uuid::Uuid::new_v4().to_string().into(),
        ctx.host_id,
        "wake-canary",
    );
    CommandContext::new(Arc::from("wake-canary"), Arc::new(request), Arc::clone(ctx))
}

fn hook_session(id: &str, cwd: &str) -> Session {
    Session {
        id: SessionId(Arc::from(id)),
        client_id: None,
        pid: 0,
        cwd: cwd.to_string(),
        git_branch: None,
        current_task: None,
        session_name: None,
        activity: None,
        kind: None,
        connected_at: 1,
        last_activity_at: Some(1),
        last_tool: None,
        last_tool_at: None,
        operator: Some("wake-canary".into()),
        host: Some(HostInfo {
            name: TEST_HOST.into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        }),
        project: Some("wake-canary".into()),
        channels_enabled: None,
    }
}

async fn wait_for_sessions(ctx: &Arc<MykoServerContext>, expected: &[&str]) {
    wait_until("hook-owned session registration", WAIT, || {
        let sessions = internal_context(ctx)
            .exec_query(GetAllSessions {})
            .unwrap_or_default();
        expected.iter().all(|expected| {
            sessions
                .iter()
                .any(|session| session.id.0.as_ref() == *expected && session.client_id.is_none())
        })
    })
    .await;
}

fn emit_message(ctx: &Arc<MykoServerContext>, id: &str, recipient: &str) {
    internal_context(ctx)
        .emit_set(&Message {
            id: MessageId(Arc::from(id)),
            from_session_id: SessionId(Arc::from("wake-canary-sender")),
            to_session_id: Some(SessionId(Arc::from(recipient))),
            to_room_id: None,
            to_operator: None,
            body: format!("wake canary {id}"),
            sent_at: 2,
        })
        .expect("emit direct message");
}

fn mark_read(ctx: &Arc<MykoServerContext>, message_id: &str, session_id: &str) {
    internal_context(ctx)
        .emit_set(&MessageRead {
            id: MessageReadId(Arc::from(MessageRead::make_id(message_id, session_id))),
            message_id: MessageId(Arc::from(message_id)),
            session_id: SessionId(Arc::from(session_id)),
            read_at: 3,
        })
        .expect("mark canary message read");
}

async fn wait_until(label: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unix_multi_process_bridge_elects_one_wake_leader_and_fails_over() {
    let temp = tempfile::tempdir().expect("test tempdir");
    let cell = CellHarness::spawn();
    let hooks = HookHarness::spawn(Arc::clone(&cell.ctx)).await;
    let app_server = FakeAppServer::spawn(&["unix-thread"]).await;

    let mut leader = BridgeProcess::spawn(&app_server, &cell, &hooks, &temp, "leader");
    leader.wait_ready().await;
    wait_for_sessions(&cell.ctx, &["unix-thread"]).await;

    // The first process acquires and settles leadership before its peer starts.
    tokio::time::sleep(Duration::from_millis(2_300)).await;
    let mut follower = BridgeProcess::spawn(&app_server, &cell, &hooks, &temp, "follower");
    follower.wait_ready().await;

    emit_message(&cell.ctx, "unix-message-1", "unix-thread");
    app_server.wait_for_count(1).await;
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        app_server.accepted(),
        vec!["unix-thread"],
        "overlapping bridge processes must create exactly one wake turn"
    );

    mark_read(&cell.ctx, "unix-message-1", "unix-thread");
    leader.stop();
    tokio::time::sleep(Duration::from_millis(250)).await;

    emit_message(&cell.ctx, "unix-message-2", "unix-thread");
    app_server.wait_for_count(2).await;
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        app_server.accepted(),
        vec!["unix-thread", "unix-thread"],
        "the follower must acquire leadership and wake after the leader exits"
    );
    mark_read(&cell.ctx, "unix-message-2", "unix-thread");
    follower.stop();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_isolated_interactive_bridges_wake_only_their_own_threads() {
    let temp = tempfile::tempdir().expect("test tempdir");
    let cell = CellHarness::spawn();
    let hooks = HookHarness::spawn(Arc::clone(&cell.ctx)).await;
    let first_app_server = FakeAppServer::spawn(&["windows-thread-a"]).await;
    let second_app_server = FakeAppServer::spawn(&["windows-thread-b"]).await;

    let mut first = BridgeProcess::spawn(&first_app_server, &cell, &hooks, &temp, "windows-a");
    let mut second = BridgeProcess::spawn(&second_app_server, &cell, &hooks, &temp, "windows-b");
    first.wait_ready().await;
    second.wait_ready().await;
    wait_for_sessions(&cell.ctx, &["windows-thread-a", "windows-thread-b"]).await;

    emit_message(&cell.ctx, "windows-message-a", "windows-thread-a");
    emit_message(&cell.ctx, "windows-message-b", "windows-thread-b");
    first_app_server.wait_for_count(1).await;
    second_app_server.wait_for_count(1).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(first_app_server.accepted(), vec!["windows-thread-a"]);
    assert_eq!(second_app_server.accepted(), vec!["windows-thread-b"]);
    mark_read(&cell.ctx, "windows-message-a", "windows-thread-a");
    mark_read(&cell.ctx, "windows-message-b", "windows-thread-b");
    first.stop();
    second.stop();
}
