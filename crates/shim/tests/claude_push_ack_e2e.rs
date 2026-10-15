//! A Claude Code shim acks each live push only once it has written it to
//! Claude, so a push the shim could not hand over stays unread for the next
//! inbox pull instead of being marked read the moment the daemon queued it.
//!
//! These launch the real `marshal-shim` against a real in-process Myko cell.
//! The shim reads whether Claude loaded Marshal's channel from its parent's
//! command line, so it is started under a `sh` whose arguments carry that
//! flag; a shim whose parent lacks it reports `channels_enabled = false` and
//! is never pushed to at all.

#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Read},
    net::SocketAddr,
    process::{Child, Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use marshal_entities::{
    AutoSource, BroadcastMessage, GetAllMessageReads, GetAllSessions, HostInfo, LivePushStatus,
    MessageId, Room, RoomId, RoomKind, RoomMember, RoomMemberId, SendMessage, Session, SessionId,
};
use myko::prelude::EventPublishing as _;
use myko::{
    command::{CommandContext, CommandHandler},
    request::RequestContext,
    server::{MykoServerContext, Persister},
};
use myko_server::{BlackholePersister, MykoServer};
use serde_json::Value;
use tempfile::TempDir;

const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");
const SENDER: &str = "push-ack-sender";
const POLL: Duration = Duration::from_millis(25);
const WAIT: Duration = Duration::from_secs(10);

struct Cell {
    address: String,
    ctx: Arc<MykoServerContext>,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl Cell {
    fn spawn() -> Self {
        marshal_entities::link();
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|listener| listener.local_addr())
            .expect("reserve port")
            .port();
        let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
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
                    result = server.run() => result.expect("run test cell"),
                    _ = tokio::task::spawn_blocking(move || shutdown_rx.recv()) => {}
                }
            });
        });
        let ctx = Arc::new(ready_rx.recv_timeout(WAIT).expect("cell context"));
        Self {
            address: format!("ws://{bind}"),
            ctx,
            shutdown: Some(shutdown_tx),
            join: Some(join),
        }
    }

    fn context(&self) -> CommandContext {
        let request = RequestContext::internal(
            uuid::Uuid::new_v4().to_string().into(),
            self.ctx.host_id,
            "push-ack",
        );
        CommandContext::new(
            Arc::from("push-ack"),
            Arc::new(request),
            Arc::clone(&self.ctx),
        )
    }

    fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.context()
            .exec_query(GetAllSessions {})
            .unwrap_or_default()
            .into_iter()
            .find(|session| session.id.0.as_ref() == id)
    }

    fn is_read(&self, message_id: &MessageId, session_id: &str) -> bool {
        self.context()
            .exec_query(GetAllMessageReads {})
            .unwrap_or_default()
            .iter()
            .any(|read| &read.message_id == message_id && read.session_id.0.as_ref() == session_id)
    }

    /// Register a hook-owned sender (no connection) to send from.
    fn add_sender(&self) {
        self.context()
            .emit_set(&Session {
                id: SessionId(Arc::from(SENDER)),
                client_id: None,
                pid: 0,
                cwd: "/".into(),
                git_branch: None,
                current_task: None,
                session_name: None,
                activity: None,
                kind: None,
                connected_at: 1,
                last_activity_at: Some(1),
                last_tool: None,
                last_tool_at: None,
                operator: Some("push-ack".into()),
                host: Some(HostInfo {
                    name: "push-ack".into(),
                    os: std::env::consts::OS.into(),
                    arch: std::env::consts::ARCH.into(),
                }),
                project: None,
                channels_enabled: None,
                acks_pushes: None,
            })
            .expect("register sender");
    }

    /// Broadcast to a room, @mentioning `to` by its nickname.
    fn mention(&self, to: &str) {
        self.context()
            .emit_set(&Room {
                id: RoomId(Arc::from("everyone")),
                name: "everyone".into(),
                description: None,
                kind: RoomKind::Auto {
                    source: AutoSource::Everyone,
                },
                created_at: 0,
            })
            .expect("create room");
        for member in [SENDER, to] {
            self.context()
                .emit_set(&RoomMember {
                    id: RoomMemberId(Arc::from(RoomMember::make_id("everyone", member))),
                    room_id: RoomId(Arc::from("everyone")),
                    session_id: SessionId(Arc::from(member)),
                    joined_at: 0,
                })
                .expect("join room");
        }
        let result = BroadcastMessage {
            to_room_id: RoomId(Arc::from("everyone")),
            body: format!("@{} have a look", marshal_entities::nickname(to)),
            as_session: Some(SessionId(Arc::from(SENDER))),
        }
        .execute(self.context())
        .expect("broadcast");
        assert_eq!(result.mentioned, vec![SessionId(Arc::from(to))]);
    }

    fn send(&self, to: &str, body: &str) -> MessageId {
        let result = SendMessage {
            to_session_id: SessionId(Arc::from(to)),
            body: body.into(),
            as_session: Some(SessionId(Arc::from(SENDER))),
        }
        .execute(self.context())
        .expect("send message");
        assert_eq!(result.live_push, LivePushStatus::Delivered, "{result:?}");
        result.message_id
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The real shim as Claude Code would run it, under a parent whose command
/// line shows Marshal's channel loaded. Killed on drop.
struct ClaudeShim {
    wrapper: Child,
    _home: TempDir,
}

impl ClaudeShim {
    fn spawn(cell: &Cell, session_id: &str) -> Self {
        let home = tempfile::tempdir().expect("shim home");
        // The trailing `:` stops the shell from exec'ing the shim in its
        // place, which would drop the flag from the shim's parent.
        let wrapper = Command::new("sh")
            .args([
                "-c",
                "\"$SHIM\"; :",
                "sh",
                "--dangerously-load-development-channels",
                "plugin:marshal-shim@marshal",
            ])
            .env("SHIM", SHIM)
            .env("HOME", home.path())
            // The daemon-address file outranks the env var, and
            // `XDG_CONFIG_HOME` is searched first: point it at the temp home
            // too, so the shim can only ever reach this test's cell.
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env_remove("MYKO_ADDRESS")
            .env("RUST_LOG", "warn")
            .env("MARSHAL_DAEMON_ADDRESS", &cell.address)
            .env("CLAUDE_CODE_SESSION_ID", session_id)
            .env_remove("MARSHAL_HARNESS")
            .current_dir(home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn marshal-shim");
        Self {
            wrapper,
            _home: home,
        }
    }

    /// Wait for the shim's own Session row to land on the roster.
    fn wait_registered(&self, cell: &Cell, session_id: &str) -> Arc<Session> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(session) = cell.session(session_id).filter(|s| s.client_id.is_some()) {
                return session;
            }
            assert!(Instant::now() < deadline, "shim never registered");
            thread::sleep(POLL);
        }
    }
}

impl Drop for ClaudeShim {
    fn drop(&mut self) {
        // Closing stdin is the shim's own exit signal.
        drop(self.wrapper.stdin.take());
        let _ = self.wrapper.kill();
        let _ = self.wrapper.wait();
    }
}

/// Lines a child writes to `stream`, read on a thread so a test can wait
/// for one with a deadline instead of blocking on a silent child forever.
fn lines_of(stream: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (line_tx, line_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            if line_tx.send(line).is_err() {
                return;
            }
        }
    });
    line_rx
}

/// The first line from `lines` that `matches`, failing the test at `WAIT`.
fn first_line(
    lines: &mpsc::Receiver<String>,
    label: &str,
    matches: impl Fn(&str) -> bool,
) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match lines.recv_timeout(left) {
            Ok(line) if matches(&line) => return line,
            Ok(_) => {}
            Err(_) => panic!("timed out waiting for {label}"),
        }
    }
}

fn wait_until(label: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        thread::sleep(POLL);
    }
}

#[test]
fn a_push_the_shim_wrote_to_claude_is_acked() {
    let cell = Cell::spawn();
    cell.add_sender();
    let me = "push-ack-written";
    let mut shim = ClaudeShim::spawn(&cell, me);
    let session = shim.wait_registered(&cell, me);
    assert_eq!(session.acks_pushes, Some(true));

    let message_id = cell.send(me, "hello from the test");

    let stdout = lines_of(shim.wrapper.stdout.take().expect("shim stdout"));
    let line = first_line(&stdout, "the channel notification", |line| {
        line.contains("notifications/claude/channel")
    });
    let pushed: Value = serde_json::from_str(&line).expect("shim wrote JSON");
    assert_eq!(
        pushed["params"]["meta"]["message_id"],
        message_id.0.as_ref()
    );
    wait_until("the shim to ack the push", || cell.is_read(&message_id, me));
}

#[test]
fn a_push_the_shim_could_not_write_stays_unread() {
    let cell = Cell::spawn();
    cell.add_sender();
    let me = "push-ack-unwritten";
    let mut shim = ClaudeShim::spawn(&cell, me);
    shim.wait_registered(&cell, me);
    // Claude stopped reading: every write to it now fails.
    drop(shim.wrapper.stdout.take());
    let stderr = lines_of(shim.wrapper.stderr.take().expect("shim stderr"));

    let message_id = cell.send(me, "nobody will see this");

    // Wait for the shim to have tried and failed, so the check below can't
    // pass just because it never got round to it.
    first_line(&stderr, "the shim to report the failed write", |line| {
        line.contains("not written to Claude") && line.contains(message_id.0.as_ref())
    });
    // An ack the shim wrongly sent would land well inside this.
    thread::sleep(Duration::from_millis(500));
    assert!(
        !cell.is_read(&message_id, me),
        "a push that never reached Claude was marked read"
    );
}

#[test]
fn a_mention_the_shim_wrote_to_claude_is_acked() {
    let cell = Cell::spawn();
    cell.add_sender();
    let me = "push-ack-mention";
    let mut shim = ClaudeShim::spawn(&cell, me);
    shim.wait_registered(&cell, me);

    cell.mention(me);

    let stdout = lines_of(shim.wrapper.stdout.take().expect("shim stdout"));
    let line = first_line(&stdout, "the mention notification", |line| {
        line.contains("notifications/claude/channel")
    });
    let pushed: Value = serde_json::from_str(&line).expect("shim wrote JSON");
    assert_eq!(pushed["params"]["meta"]["kind"], "mention");
    let ping = MessageId(Arc::from(
        pushed["params"]["meta"]["message_id"]
            .as_str()
            .expect("the mention names its ping"),
    ));
    wait_until("the shim to ack the mention", || cell.is_read(&ping, me));
}
