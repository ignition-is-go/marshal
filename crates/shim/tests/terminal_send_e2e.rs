//! `marshal-shim tell` from two terminals at once: each run registers its own
//! short-lived Session, so the first to finish can't take the other's sender
//! off the roster.
//!
//! These launch the real `marshal-shim` against a real in-process Myko cell.

#![cfg(unix)]

use std::{
    net::SocketAddr,
    path::Path,
    process::{Command, Output},
    sync::Arc,
    thread,
    time::Duration,
};

use marshal_entities::{GetAllMessages, GetAllSessions, HostInfo, Session, SessionId};
use myko::prelude::EventPublishing as _;
use myko::{
    command::CommandContext,
    request::RequestContext,
    server::{MykoServerContext, Persister},
};
use myko_server::{BlackholePersister, MykoServer};

const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");
const RECIPIENT: &str = "terminal-e2e-recipient-0001";
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
            "terminal-send",
        );
        CommandContext::new(
            Arc::from("terminal-send"),
            Arc::new(request),
            Arc::clone(&self.ctx),
        )
    }

    /// A session to send to. It has no live client, so the mail just waits.
    fn add_recipient(&self) {
        self.context()
            .emit_set(&Session {
                id: SessionId(Arc::from(RECIPIENT)),
                client_id: None,
                agent_id: None,
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
                operator: Some("terminal-send".into()),
                host: Some(HostInfo {
                    name: "terminal-send".into(),
                    os: std::env::consts::OS.into(),
                    arch: std::env::consts::ARCH.into(),
                }),
                project: None,
                channels_enabled: None,
                acks_pushes: None,
            })
            .expect("register recipient");
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

/// `marshal-shim tell` with its own config directory, so no address file from
/// this machine is read: the cell's address comes from the environment.
fn tell(address: &str, home: &Path, body: &str) -> Output {
    Command::new(SHIM)
        .args(["tell", RECIPIENT, body])
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("APPDATA", home.join("appdata"))
        .env("MARSHAL_DAEMON_ADDRESS", address)
        .env_remove("MYKO_ADDRESS")
        .output()
        .expect("run marshal-shim tell")
}

#[test]
fn two_terminal_runs_at_once_each_send_from_their_own_session() {
    let cell = Cell::spawn();
    cell.add_recipient();
    // Both runs share one config directory, as two terminals on one account do.
    let home = tempfile::tempdir().expect("terminal home");

    let runs: Vec<_> = ["first terminal", "second terminal"]
        .into_iter()
        .map(|body| {
            let address = cell.address.clone();
            let home = home.path().to_path_buf();
            thread::spawn(move || tell(&address, &home, body))
        })
        .collect();
    for run in runs {
        let out = run.join().expect("terminal thread");
        assert!(
            out.status.success(),
            "a terminal run failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let messages = cell
        .context()
        .exec_query(GetAllMessages {})
        .expect("read messages");
    let senders: Vec<&str> = ["first terminal", "second terminal"]
        .iter()
        .map(|body| {
            messages
                .iter()
                .find(|m| m.body.ends_with(body))
                .unwrap_or_else(|| panic!("no message ending {body:?}: {messages:?}"))
                .from_session_id
                .0
                .as_ref()
        })
        .collect();
    assert_ne!(
        senders[0], senders[1],
        "both runs sent from one session, so the first to exit removes the other's"
    );

    // Each run removes its own row on exit; none is left behind.
    let terminals: Vec<_> = cell
        .context()
        .exec_query(GetAllSessions {})
        .expect("read roster")
        .into_iter()
        .filter(|s| s.kind.as_deref() == Some("terminal"))
        .collect();
    assert!(terminals.is_empty(), "terminal rows left: {terminals:?}");
}

#[test]
fn a_later_run_from_the_same_terminal_gets_a_new_session() {
    let cell = Cell::spawn();
    cell.add_recipient();
    let home = tempfile::tempdir().expect("terminal home");
    for body in ["run one", "run two"] {
        let out = tell(&cell.address, home.path(), body);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let messages = cell
        .context()
        .exec_query(GetAllMessages {})
        .expect("read messages");
    let from = |body: &str| {
        messages
            .iter()
            .find(|m| m.body.ends_with(body))
            .map(|m| m.from_session_id.0.to_string())
            .unwrap_or_else(|| panic!("no message ending {body:?}"))
    };
    assert_ne!(from("run one"), from("run two"));
    // Nothing is written to keep an id between runs.
    assert!(
        !home.path().join("config/marshal/terminal-session").exists(),
        "a per-machine terminal id file was written"
    );
}
