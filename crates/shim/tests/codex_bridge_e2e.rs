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
use hyphae::Gettable;
use marshal_entities::{
    GetAllMessageReads, GetAllMessages, GetAllSessions, HostInfo, Message, MessageId, MessageRead,
    MessageReadId, Session, SessionId,
};
use myko::prelude::EventPublishing as _;
use myko::{command::CommandContext, request::RequestContext, server::MykoServerContext};
use myko_server::{BlackholePersister, MykoServer};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message as WebSocketMessage};

const TEST_HOST: &str = "marshal-wake-canary";
const POLL: Duration = Duration::from_millis(25);
const WAIT: Duration = Duration::from_secs(8);
const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");

struct CellHarness {
    address: String,
    ctx: Arc<MykoServerContext>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl CellHarness {
    fn spawn(first_addr: SocketAddr) -> Self {
        marshal_entities::link();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        let join = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("build cell runtime");
            runtime.block_on(async move {
                let addresses = std::iter::once(first_addr)
                    .chain(std::iter::repeat_with(candidate_addr))
                    .take(5);
                for bind in addresses {
                    let server = MykoServer::builder()
                        .with_bind_addr(bind)
                        .with_websocket_path("/myko")
                        .with_default_persister(Arc::new(BlackholePersister))
                        .build();
                    let address = format!("ws://{bind}");
                    let run = server.run();
                    tokio::pin!(run);

                    tokio::select! {
                        biased;
                        result = &mut run => {
                            let error = result.expect_err("test cell stopped during startup");
                            if error.downcast_ref::<io::Error>().is_some_and(|error| {
                                error.kind() == io::ErrorKind::AddrInUse
                            }) {
                                continue;
                            }
                            panic!("run test cell: {error}");
                        }
                        () = wait_for_cell(&address) => {}
                        _ = &mut shutdown_rx => return,
                    }

                    ready_tx
                        .send((address, server.ctx()))
                        .expect("publish ready cell");
                    tokio::select! {
                        result = &mut run => result.expect("run test cell"),
                        _ = &mut shutdown_rx => {}
                    }
                    return;
                }
                panic!("could not bind test cell after five attempts");
            });
        });
        let (address, ctx) = ready_rx.recv_timeout(WAIT).expect("ready cell");

        Self {
            address,
            ctx: Arc::new(ctx),
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
    async fn spawn_daemon(ctx: Arc<MykoServerContext>) -> Self {
        let address = candidate_addr();
        let task =
            tokio::spawn(async move { daemon::http_listener::run(address, ctx).await.unwrap() });
        let deadline = Instant::now() + WAIT;
        loop {
            if TcpStream::connect(address).await.is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "daemon hook listener readiness");
            tokio::time::sleep(POLL).await;
        }
        Self {
            base_url: format!("http://{address}"),
            task,
        }
    }

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
    if target == "/hook/inbox?ack=client" || target == "/hook/ack" {
        let body: Value = serde_json::from_slice(&body)?;
        let session = body["session_id"].as_str().unwrap();
        if target == "/hook/ack" {
            for id in body["message_ids"].as_array().unwrap() {
                mark_read(&ctx, id.as_str().unwrap(), session);
            }
        } else {
            let messages = internal_context(&ctx)
                .exec_query(GetAllMessages {})
                .unwrap();
            let pending: Vec<_> = messages
                .iter()
                .filter(|message| {
                    message
                        .to_session_id
                        .as_ref()
                        .is_some_and(|id| id.0.as_ref() == session)
                        && !is_read(&ctx, message.id.0.as_ref(), session)
                })
                .collect();
            let ids: Vec<_> = pending
                .iter()
                .map(|message| message.id.0.as_ref())
                .collect();
            let inbox = pending
                .iter()
                .map(|message| message.body.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let header = format!("X-Marshal-Ack-V1: {}\r\n", json!(ids));
            stream.write_all(format!("HTTP/1.1 200 OK\r\n{header}Content-Length: {}\r\nConnection: close\r\n\r\n{inbox}", inbox.len()).as_bytes()).await?;
            return stream.flush().await;
        }
    }
    if target.starts_with("/hook/session-register")
        && let Ok(body) = serde_json::from_slice::<Value>(&body)
        && let Some(session_id) = body.get("session_id").and_then(Value::as_str)
    {
        let cwd = body
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("/wake-canary");
        assert!(!target.contains("agent_id"));
        assert_eq!(body["agent_id"], "bridge/\"agent\nopaque");
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

    let mut wake = None;
    while let Some(frame) = websocket.next().await {
        let frame = frame?;
        let WebSocketMessage::Text(text) = frame else {
            continue;
        };
        let request: Value = serde_json::from_str(text.as_ref())?;
        if request["method"] == "thread/loaded/list" {
            websocket.send(WebSocketMessage::Text(json!({"id":request["id"],"result":{"data":owned_threads.iter().collect::<Vec<_>>(),"nextCursor":null}}).to_string().into())).await?;
            continue;
        }
        if request["method"] == "thread/read" {
            let thread = request["params"]["threadId"].as_str().unwrap();
            let status = if owned_threads.contains(thread) {
                "idle"
            } else {
                "notLoaded"
            };
            let result = json!({
                "id":request["id"],
                "result":{"thread":{"id":thread,"sessionId":thread,"cwd":format!("/wake-canary/{thread}"),"status":{"type":status}}}
            });
            websocket
                .send(WebSocketMessage::Text(result.to_string().into()))
                .await?;
            continue;
        }
        if request["method"] == "thread/turns/list" {
            let (turn, failed) = wake.as_ref().expect("completion follows a wake");
            let status = if *failed { "failed" } else { "completed" };
            let result = json!({"id":request["id"],"result":{"data":[{"id":"unrelated","status":"completed"},{"id":turn,"status":status}]}});
            websocket
                .send(WebSocketMessage::Text(result.to_string().into()))
                .await?;
            continue;
        }
        if request["method"] != "turn/start" {
            continue;
        }
        let id = request.get("id").cloned().unwrap_or(json!(2));
        let thread = request
            .pointer("/params/threadId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let response = if owned_threads.contains(thread) {
            assert_eq!(request["params"]["input"], json!([]));
            assert_eq!(
                request["params"]["additionalContext"]["marshal-wakeup"],
                json!({
                    "kind": "application",
                    "value": "Marshal started this turn to deliver direct messages. Read the \
                              `marshal_inbox` tool output and handle it under the current task and authorization."
                })
            );
            let inbox = &request["params"]["additionalContext"]["marshal-inbox"];
            assert_eq!(inbox["kind"], "untrusted");
            let content = inbox["value"].as_str().unwrap();
            assert!(content.contains("wake canary"));
            wake = Some((format!("turn-{thread}"), content.contains("failed-message")));
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
        Self::spawn_bound(app_server, cell, hooks, temp, name, None)
    }

    fn spawn_bound(
        app_server: &FakeAppServer,
        cell: &CellHarness,
        hooks: &HookHarness,
        temp: &TempDir,
        name: &str,
        selected_thread: Option<&str>,
    ) -> Self {
        let ready_file = temp.path().join(format!("{name}.ready"));
        let mut command = Command::new(SHIM);
        command.arg("codex-bridge");
        if let Some(thread) = selected_thread {
            command
                .env("MARSHAL_LAUNCH_ID", "managed-presence-canary")
                .args([
                    "--launcher-thread-id",
                    thread,
                    "--launcher-cwd",
                    "/wake-canary",
                    "--launcher-state-file",
                ])
                .arg(temp.path().join(format!("{name}.state.json")));
        }
        command
            .args([
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
            .env("MARSHAL_AGENT_ID", " bridge/\"agent\nopaque ")
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

async fn wait_for_cell(address: &str) {
    loop {
        match connect_async(format!("{address}/myko")).await {
            Ok(_) => return,
            Err(tokio_tungstenite::tungstenite::Error::Io(error))
                if error.kind() == io::ErrorKind::ConnectionRefused =>
            {
                tokio::time::sleep(POLL).await;
            }
            Err(error) => panic!("connect to test cell: {error}"),
        }
    }
}

fn candidate_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve port");
    listener.local_addr().expect("candidate address")
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
        managed_presence: false,
        client_id: None,
        agent_id: None,
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
        acks_pushes: None,
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
            response_required: false,
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

fn is_read(ctx: &Arc<MykoServerContext>, message: &str, session: &str) -> bool {
    internal_context(ctx)
        .exec_query(GetAllMessageReads {})
        .unwrap()
        .iter()
        .any(|read| read.message_id.0.as_ref() == message && read.session_id.0.as_ref() == session)
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
    let occupied = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind competing listener");
    let cell = CellHarness::spawn(occupied.local_addr().expect("competing address"));
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

    wait_until("successful wake acknowledged", WAIT, || {
        is_read(&cell.ctx, "unix-message-1", "unix-thread")
    })
    .await;
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
    wait_until("successful wake acknowledged", WAIT, || {
        is_read(&cell.ctx, "unix-message-2", "unix-thread")
    })
    .await;
    follower.stop();

    let failed_server = FakeAppServer::spawn(&["failed-thread"]).await;
    let mut failed_bridge = BridgeProcess::spawn(&failed_server, &cell, &hooks, &temp, "failed");
    failed_bridge.wait_ready().await;
    wait_for_sessions(&cell.ctx, &["failed-thread"]).await;
    emit_message(&cell.ctx, "failed-message", "failed-thread");
    failed_server.wait_for_count(1).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!is_read(&cell.ctx, "failed-message", "failed-thread"));
    failed_bridge.stop();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_restores_deleted_initial_launch_session_without_started_notification() {
    let temp = tempfile::tempdir().unwrap();
    let cell = CellHarness::spawn(candidate_addr());
    let mut unrelated = hook_session("unrelated-thread", "/unrelated");
    unrelated.agent_id = Some("unrelated-owner".into());
    unrelated.session_name = Some("unrelated title".into());
    internal_context(&cell.ctx).emit_set(&unrelated).unwrap();
    let hook_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let hook_address = hook_listener.local_addr().unwrap();
    let ctx = Arc::clone(&cell.ctx);
    let hooks = HookHarness {
        base_url: format!("http://{hook_address}"),
        task: tokio::spawn(async move {
            loop {
                let (mut stream, _) = hook_listener.accept().await.unwrap();
                let (target, body) = read_http_request(&mut stream).await.unwrap();
                assert!(target.starts_with("/hook/session-register?"));
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["session_id"], "initial-thread");
                assert_eq!(body["agent_id"], "bridge/\"agent\nopaque");
                let mut session = hook_session("initial-thread", "/initial");
                session.agent_id = body["agent_id"].as_str().map(str::to_string);
                internal_context(&ctx).emit_set(&session).unwrap();
                let body = json!({"session_id":"initial-thread", "nickname": marshal_entities::nickname("initial-thread")}).to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        }),
    };
    let app_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let app_address = app_listener.local_addr().unwrap();
    let name = Arc::new(Mutex::new("literal-name".to_string()));
    let name_sets = Arc::new(Mutex::new(Vec::<String>::new()));
    let (disconnect, _) = tokio::sync::broadcast::channel::<()>(1);
    let disconnect_for_server = disconnect.clone();
    let (rename, _) = tokio::sync::broadcast::channel::<String>(1);
    let rename_for_server = rename.clone();
    let name_for_server = Arc::clone(&name);
    let sets_for_server = Arc::clone(&name_sets);
    let app_server = FakeAppServer {
        endpoint: format!("ws://{app_address}"),
        accepted: Arc::new(Mutex::new(Vec::new())),
        task: tokio::spawn(async move {
            loop {
                let (stream, _) = app_listener.accept().await.unwrap();
                let mut disconnect = disconnect_for_server.subscribe();
                let mut rename = rename_for_server.subscribe();
                let name = Arc::clone(&name_for_server);
                let name_sets = Arc::clone(&sets_for_server);
                tokio::spawn(async move {
                    let mut websocket = accept_async(stream).await.unwrap();
                    loop {
                        let frame = tokio::select! {
                            _ = disconnect.recv() => break,
                            Ok(updated) = rename.recv() => {
                                let notification = json!({
                                    "method": "thread/name/updated",
                                    "params": {"threadId": "initial-thread", "threadName": updated}
                                });
                                if websocket.send(WebSocketMessage::Text(notification.to_string().into())).await.is_err() {
                                    break;
                                }
                                continue;
                            }
                            frame = websocket.next() => frame,
                        };
                        let Some(Ok(WebSocketMessage::Text(text))) = frame else {
                            break;
                        };
                        let request: Value = serde_json::from_str(text.as_ref()).unwrap();
                        let Some(id) = request.get("id") else {
                            continue;
                        };
                        let result = match request["method"].as_str().unwrap() {
                            "initialize" => {
                                json!({"userAgent":"recovery-test","platformFamily":"test","platformOs":"linux","codexHome":"/tmp"})
                            }
                            "thread/loaded/list" => {
                                json!({"data":["initial-thread"],"nextCursor":null})
                            }
                            "thread/list" => json!({"data":[
                                {"id":"initial-thread","status":{"type":"notLoaded"}},
                                {"id":"unrelated-thread","status":{"type":"loaded"}}
                            ],"nextCursor":null}),
                            "thread/read" | "thread/resume" => {
                                let thread = request["params"]["threadId"].as_str().unwrap();
                                json!({"thread":{"id":thread,"sessionId":thread,"cwd":"/initial","name":name.lock().unwrap().clone(),"status":{"type":"idle"}}})
                            }
                            "thread/name/set" => {
                                assert_eq!(request["params"]["threadId"], "initial-thread");
                                let updated =
                                    request["params"]["name"].as_str().unwrap().to_string();
                                *name.lock().unwrap() = updated.clone();
                                name_sets.lock().unwrap().push(updated);
                                json!({})
                            }
                            method => panic!("unexpected recovery RPC {method}"),
                        };
                        if websocket
                            .send(WebSocketMessage::Text(
                                json!({"id":id,"result":result}).to_string().into(),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        }),
    };
    let proxy = format!("ws://{}", candidate_addr());
    let state_file = temp.path().join("launcher.json");
    let ready_file = temp.path().join("ready");
    let child = Command::new(SHIM)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("MARSHAL_LAUNCH_ID", "bridge/\"agent\nopaque")
        .env("MARSHAL_BASE_URL", &hooks.base_url)
        .env("MARSHAL_HOST", TEST_HOST)
        .args([
            "codex-bridge",
            "--endpoint",
            &app_server.endpoint,
            "--proxy-endpoint",
            &proxy,
            "--daemon",
            &cell.address,
            "--launcher-thread-id",
            "initial-thread",
            "--launcher-cwd",
            "/initial",
            "--launcher-state-file",
        ])
        .arg(&state_file)
        .arg("--ready-file")
        .arg(&ready_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut bridge = BridgeProcess {
        child: Some(child),
        ready_file,
    };
    bridge.wait_ready().await;
    let decorated = format!(
        "[{}] literal-name",
        marshal_entities::nickname("initial-thread")
    );
    for generation in 1..=3 {
        if generation > 1 {
            internal_context(&cell.ctx)
                .emit_del(&hook_session("initial-thread", ""))
                .unwrap();
            assert!(
                internal_context(&cell.ctx)
                    .exec_query(GetAllSessions {})
                    .unwrap()
                    .iter()
                    .all(|row| row.id.0.as_ref() != "initial-thread")
            );
            disconnect.send(()).unwrap();
        }
        wait_until("new lifecycle generation", WAIT, || {
            std::fs::read(&state_file)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some_and(|state| {
                    state["generation"].as_u64().unwrap_or(0) >= generation
                        && state["connected"] == true
                })
        })
        .await;
        let (mut tui, _) = connect_async(&proxy).await.unwrap();
        tui.send(WebSocketMessage::Text(
            json!({"id":1,"method":"initialize","params":{}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        tui.next().await.unwrap().unwrap();
        tui.send(WebSocketMessage::Text(
            json!({"method":"initialized"}).to_string().into(),
        ))
        .await
        .unwrap();
        tui.send(WebSocketMessage::Text(
            json!({"id":2,"method":"thread/resume","params":{"threadId":"initial-thread"}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let reply = tui.next().await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(reply.to_text().unwrap()).unwrap()["result"]["thread"]["id"],
            "initial-thread"
        );
        wait_for_sessions(&cell.ctx, &["initial-thread"]).await;
        wait_until("initial name decoration", WAIT, || {
            *name.lock().unwrap() == decorated
        })
        .await;
        wait_until("roster title projection", WAIT, || {
            internal_context(&cell.ctx)
                .exec_query(GetAllSessions {})
                .unwrap()
                .iter()
                .any(|row| {
                    row.id.0.as_ref() == "initial-thread"
                        && row.session_name.as_deref() == Some("literal-name")
                })
        })
        .await;
        let sessions = internal_context(&cell.ctx)
            .exec_query(GetAllSessions {})
            .unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions
                .iter()
                .find(|row| row.id.0.as_ref() == "initial-thread")
                .unwrap()
                .agent_id
                .as_deref(),
            Some("bridge/\"agent\nopaque")
        );
        let unrelated = sessions
            .iter()
            .find(|row| row.id.0.as_ref() == "unrelated-thread")
            .unwrap();
        assert_eq!(unrelated.agent_id.as_deref(), Some("unrelated-owner"));
        assert_eq!(unrelated.session_name.as_deref(), Some("unrelated title"));
        assert_eq!(
            name_sets.lock().unwrap().as_slice(),
            std::slice::from_ref(&decorated)
        );
        tokio::time::sleep(POLL).await;
    }
    let renamed = "Renamed in Codex";
    *name.lock().unwrap() = renamed.into();
    rename.send(renamed.into()).unwrap();
    wait_until("Codex rename reaches roster", WAIT, || {
        let rows = internal_context(&cell.ctx)
            .exec_query(GetAllSessions {})
            .unwrap();
        rows.iter().any(|row| {
            row.id.0.as_ref() == "initial-thread" && row.session_name.as_deref() == Some(renamed)
        })
    })
    .await;
    bridge.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connected_idle_bridge_survives_activity_backstop_after_successful_wake() {
    let temp = tempfile::tempdir().expect("test tempdir");
    let cell = CellHarness::spawn(candidate_addr());
    let hooks = HookHarness::spawn_daemon(Arc::clone(&cell.ctx)).await;
    let app_server = FakeAppServer::spawn(&["idle-managed-thread"]).await;
    let mut bridge = BridgeProcess::spawn_bound(
        &app_server,
        &cell,
        &hooks,
        &temp,
        "idle-managed",
        Some("idle-managed-thread"),
    );
    bridge.wait_ready().await;
    wait_for_sessions(&cell.ctx, &["idle-managed-thread"]).await;

    let mut aged = internal_context(&cell.ctx)
        .exec_query(GetAllSessions {})
        .unwrap()
        .into_iter()
        .find(|row| row.id.0.as_ref() == "idle-managed-thread")
        .map(|row| (*row).clone())
        .unwrap();
    aged.last_activity_at = Some(
        chrono::Utc::now().timestamp_millis()
            - daemon::cleanup::PULL_STALE_AFTER.as_millis() as i64
            - 60_000,
    );
    aged.connected_at = aged.last_activity_at.unwrap();
    aged.last_tool_at = None;
    internal_context(&cell.ctx).emit_set(&aged).unwrap();

    emit_message(&cell.ctx, "idle-message", "idle-managed-thread");
    app_server.wait_for_count(1).await;
    wait_until("successful wake acknowledged", WAIT, || {
        is_read(&cell.ctx, "idle-message", "idle-managed-thread")
    })
    .await;
    let registered = internal_context(&cell.ctx)
        .exec_query(GetAllSessions {})
        .unwrap();
    let session = registered
        .iter()
        .find(|row| row.id.0.as_ref() == "idle-managed-thread")
        .unwrap();
    assert_eq!(session.agent_id.as_deref(), Some("managed-presence-canary"));
    eprintln!(
        "before sweep: client_id={:?}, last_activity_at={:?}, wake acknowledged=true",
        session.client_id, session.last_activity_at
    );

    let sweeper = tokio::spawn(daemon::cleanup::run_sweeper((*cell.ctx).clone()));
    tokio::time::sleep(daemon::cleanup::TICK_INTERVAL + Duration::from_millis(250)).await;
    sweeper.abort();
    assert!(
        bridge.child.as_mut().unwrap().try_wait().unwrap().is_none(),
        "bridge must remain alive during sweep"
    );
    let rows = internal_context(&cell.ctx)
        .exec_query(GetAllSessions {})
        .unwrap();
    assert!(
        rows.iter()
            .any(|row| row.id.0.as_ref() == "idle-managed-thread"),
        "connected managed bridge lost its roster row at the activity backstop after acknowledging a wake"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connected_bridge_repairs_manually_deleted_roster_row_without_reconnect() {
    let temp = tempfile::tempdir().expect("test tempdir");
    let cell = CellHarness::spawn(candidate_addr());
    let hooks = HookHarness::spawn_daemon(Arc::clone(&cell.ctx)).await;
    let app_server = FakeAppServer::spawn(&["deleted-managed-thread"]).await;
    let mut bridge = BridgeProcess::spawn_bound(
        &app_server,
        &cell,
        &hooks,
        &temp,
        "deleted-managed",
        Some("deleted-managed-thread"),
    );
    bridge.wait_ready().await;
    wait_for_sessions(&cell.ctx, &["deleted-managed-thread"]).await;
    assert!(bridge.child.as_mut().unwrap().try_wait().unwrap().is_none());
    cell.ctx
        .del_by_id("Session", "deleted-managed-thread")
        .unwrap();
    assert!(
        !internal_context(&cell.ctx)
            .exec_query(GetAllSessions {})
            .unwrap()
            .iter()
            .any(|row| row.id.0.as_ref() == "deleted-managed-thread")
    );
    wait_for_sessions(&cell.ctx, &["deleted-managed-thread"]).await;
    assert!(
        bridge.child.as_mut().unwrap().try_wait().unwrap().is_none(),
        "bridge must remain alive during repair"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measures the real 90-second lease plus 60-second grace"]
async fn suspended_publisher_expires_with_its_websocket_still_connected() {
    use marshal_entities::{
        GetAllManagedPresenceOwners, ManagedThreadRegistration, ReconcileManagedPresence,
    };
    use myko::client::{ConnectionStatus, MykoClient};
    let cell = CellHarness::spawn(candidate_addr());
    let client = MykoClient::new();
    client.set_address(Some(cell.address.clone()));
    wait_until("publisher socket connected", WAIT, || {
        matches!(
            client.connection_status_sync(),
            ConnectionStatus::Connected(_)
        )
    })
    .await;
    let threads = vec![ManagedThreadRegistration {
        session_id: SessionId("suspended".into()),
        thread_id: "suspended".into(),
        cwd: "/owned".into(),
        title: None,
        agent_id: None,
        operator: None,
        host: None,
    }];
    let first = client.send_command::<ReconcileManagedPresence, ()>(&ReconcileManagedPresence {
        sequence: 1,
        threads: threads.clone(),
        retire: false,
    });
    wait_until("first claim accepted", WAIT, || first.get().is_some()).await;
    first.get().unwrap().unwrap();
    let started = Instant::now();
    let sweeper = tokio::spawn(daemon::cleanup::run_sweeper((*cell.ctx).clone()));
    tokio::time::sleep(Duration::from_secs(94)).await;
    assert!(matches!(
        client.connection_status_sync(),
        ConnectionStatus::Connected(_)
    ));
    let late = client.send_command::<ReconcileManagedPresence, ()>(&ReconcileManagedPresence {
        sequence: 2,
        threads,
        retire: false,
    });
    wait_until("expired incarnation rejected", WAIT, || {
        late.get().is_some()
    })
    .await;
    assert!(
        late.get()
            .unwrap()
            .unwrap_err()
            .contains("expired or retired")
    );
    let projection = client.send_command::<marshal_entities::ReadManagedPresence, marshal_entities::ManagedPresenceSnapshot>(&marshal_entities::ReadManagedPresence {});
    wait_until("expired presence projection answered", WAIT, || {
        projection.get().is_some()
    })
    .await;
    assert!(
        projection.get().unwrap().unwrap().sessions.is_empty(),
        "an expired claim is not live even before Session grace ends"
    );
    wait_until(
        "suspended publisher session reaped",
        Duration::from_secs(65),
        || {
            !internal_context(&cell.ctx)
                .exec_query(GetAllSessions {})
                .unwrap()
                .iter()
                .any(|row| row.id.0.as_ref() == "suspended")
        },
    )
    .await;
    assert!(matches!(
        client.connection_status_sync(),
        ConnectionStatus::Connected(_)
    ));
    assert!(
        started.elapsed() <= Duration::from_secs(154),
        "expiry exceeded 90+60+3-second detection bound"
    );
    let owners = internal_context(&cell.ctx)
        .exec_query(GetAllManagedPresenceOwners {})
        .unwrap();
    assert_eq!(owners.len(), 1);
    assert!(owners[0].retired);
    eprintln!(
        "publisher stopped renewing, socket stayed connected, Session removed after {:?}",
        started.elapsed()
    );
    sweeper.abort();
    client.set_address(None);
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_isolated_interactive_bridges_wake_only_their_own_threads() {
    let temp = tempfile::tempdir().expect("test tempdir");
    let cell = CellHarness::spawn(candidate_addr());
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
    wait_until("successful wake acknowledged", WAIT, || {
        is_read(&cell.ctx, "windows-message-a", "windows-thread-a")
    })
    .await;
    wait_until("successful wake acknowledged", WAIT, || {
        is_read(&cell.ctx, "windows-message-b", "windows-thread-b")
    })
    .await;
    first.stop();
    second.stop();
}
