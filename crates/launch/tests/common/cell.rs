//! An in-process Myko cell carrying Marshal's entities and command handlers,
//! for tests that need a daemon's roster and mail but not its Pulse-backed parts.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use marshal_entities::{GetAllSessions, HostInfo, Session};
use myko::prelude::EventPublishing as _;
use myko::{
    command::CommandContext,
    request::RequestContext,
    server::{MykoServerContext, Persister},
};
use myko_server::{BlackholePersister, MykoServer};

const WAIT: Duration = Duration::from_secs(10);

pub struct Cell {
    pub address: String,
    ctx: Arc<MykoServerContext>,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl Cell {
    pub fn spawn() -> Self {
        Self::spawn_on("127.0.0.1")
    }

    pub fn spawn_on(ip: &str) -> Self {
        marshal_entities::link();
        let port = free_port();
        let bind: SocketAddr = format!("{ip}:{port}").parse().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
        let join = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
                let server = Arc::new(
                    MykoServer::builder()
                        .with_bind_addr(bind)
                        .with_default_persister(blackhole)
                        .build(),
                );
                ready_tx.send(server.ctx()).unwrap();
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

    pub fn context(&self) -> CommandContext {
        let request = RequestContext::internal(
            uuid::Uuid::new_v4().to_string().into(),
            self.ctx.host_id,
            "launch-e2e",
        );
        CommandContext::new(
            Arc::from("launch-e2e"),
            Arc::new(request),
            Arc::clone(&self.ctx),
        )
    }

    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.context()
            .exec_query(GetAllSessions {})
            .unwrap_or_default()
    }

    /// Register a row as an agent's hooks would.
    pub fn register(&self, id: &str, agent_id: &str, host: &str, cwd: &str) {
        let mut s: Session = serde_json::from_value(serde_json::json!({
            "id": id,
            "pid": 1,
            "cwd": cwd,
            "connectedAt": 1,
            "agentId": agent_id,
        }))
        .unwrap();
        s.host = Some(HostInfo {
            name: host.into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        });
        self.context().emit_set(&s).expect("register row");
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        if let Some(s) = self.shutdown.take() {
            let _ = s.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .unwrap()
        .port()
}
