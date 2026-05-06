//! Integration test for the WS reconnect bug.
//!
//! Symptom: web GUI's session list goes stale after a daemon restart;
//! SETs that fire after the daemon comes back up don't reach an
//! already-open client. Reproducer here:
//!
//!   1. Spin up CellServer V1 on a port (own tokio runtime in its
//!      own thread, so we can kill the entire runtime to mimic a
//!      process exit — aborting the JoinHandle of `server.run()`
//!      alone wouldn't close per-connection tasks, since they're
//!      detached from the accept loop).
//!   2. Connect a `MykoClient` and `watch_query::<GetAllSessions>()`.
//!   3. SET a session via V1's ctx — verify the client receives it.
//!   4. Shut down V1's runtime (simulates `kill` of the daemon).
//!   5. Wait for the client's autosocket to drop into Disconnected.
//!   6. Spin up CellServer V2 on the same port.
//!   7. SET another session via V2's ctx and assert the client's cell
//!      contains it.

use std::{
    net::SocketAddr,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use entities::{GetAllSessions, Session, SessionId};
use hyphae::Gettable;
use myko::{
    client::{ConnectionStatus, MykoClient, MykoProtocol},
    server::{CellServerCtx, Persister},
    wire::{MEvent, MEventType},
};
use myko_server::{BlackholePersister, CellServer};

const POLL_TIMEOUT: Duration = Duration::from_secs(8);

/// Handle for a server running on its own tokio runtime in its own
/// thread. `shutdown()` tears the runtime down completely (which
/// closes all per-connection sockets, simulating `kill` of the
/// daemon process).
struct ServerHandle {
    ctx: CellServerCtx,
    shutdown: Option<mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl ServerHandle {
    fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn spawn_server(bind: SocketAddr) -> ServerHandle {
    let (ready_tx, ready_rx) = mpsc::sync_channel::<CellServerCtx>(1);
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let join = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
            let server = Arc::new(
                CellServer::builder()
                    .with_bind_addr(bind)
                    .with_default_persister(blackhole)
                    .build(),
            );
            ready_tx.send(server.ctx()).expect("send ctx");
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::task::spawn_blocking(move || {
                    let _ = shutdown_rx.recv();
                }) => {}
            }
        });
        drop(rt);
    });

    let ctx = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server thread came up");

    ServerHandle {
        ctx,
        shutdown: Some(shutdown_tx),
        join: Some(join),
    }
}

fn session_set_event(id: &str, nickname: &str) -> MEvent {
    let session = Session {
        id: SessionId(Arc::from(id)),
        client_id: None,
        nickname: nickname.into(),
        pid: 0,
        cwd: "/tmp".into(),
        git_branch: None,
        current_task: None,
        connected_at: chrono::Utc::now().timestamp_millis(),
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
    };
    MEvent::from_item(&session, MEventType::SET, &uuid::Uuid::new_v4().to_string())
}

fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("pick free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn wait_for(label: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + POLL_TIMEOUT;
    while Instant::now() < deadline {
        if f() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for: {label}");
}

#[test]
fn client_subscription_resumes_after_server_restart() {
    let _ = env_logger::builder().is_test(true).try_init();
    entities::link();
    daemon::link();

    let port = pick_free_port();
    let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let addr = format!("ws://{bind}");

    let server_v1 = spawn_server(bind);

    let client = MykoClient::new();
    client.set_protocol(MykoProtocol::JSON);
    client.set_address(Some(addr.clone()));

    let status = client.connection_status();
    let s1 = status.clone();
    wait_for("initial connection", move || {
        matches!(s1.get(), ConnectionStatus::Connected(_))
    });

    let cell = client.watch_query::<GetAllSessions>(GetAllSessions {});

    server_v1
        .ctx
        .apply_event_batch(vec![session_set_event("s-pre", "before-restart")])
        .expect("apply pre-restart session");

    let c1 = cell.clone();
    wait_for("pre-restart session on client", move || {
        c1.get().iter().any(|s| s.id.0.as_ref() == "s-pre")
    });

    server_v1.shutdown();

    let s2 = status.clone();
    wait_for("client observes disconnect", move || {
        !matches!(s2.get(), ConnectionStatus::Connected(_))
    });

    let mut server_v2 = None;
    for _ in 0..30 {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| spawn_server(bind))) {
            Ok(s) => {
                server_v2 = Some(s);
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    let server_v2 = server_v2.expect("rebind same port for server v2");

    let s3 = status.clone();
    wait_for("client reconnects", move || {
        matches!(s3.get(), ConnectionStatus::Connected(_))
    });

    thread::sleep(Duration::from_millis(500));

    server_v2
        .ctx
        .apply_event_batch(vec![session_set_event("s-post", "after-restart")])
        .expect("apply post-restart session");

    let c2 = cell.clone();
    wait_for("post-restart session on client", move || {
        c2.get().iter().any(|s| s.id.0.as_ref() == "s-post")
    });

    server_v2.shutdown();
}

/// Same dance, but with multiple concurrent subscriptions — same shape
/// as the web app, which subscribes to GetAllSessions and GetAllMessages.
#[test]
fn many_subscriptions_resume_together_after_restart() {
    let _ = env_logger::builder().is_test(true).try_init();
    entities::link();
    daemon::link();

    let port = pick_free_port();
    let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let addr = format!("ws://{bind}");

    let server_v1 = spawn_server(bind);

    let client = MykoClient::new();
    client.set_protocol(MykoProtocol::JSON);
    client.set_address(Some(addr.clone()));
    let status = client.connection_status();
    let s = status.clone();
    wait_for("initial connection", move || {
        matches!(s.get(), ConnectionStatus::Connected(_))
    });

    let cells: Vec<_> = (0..5)
        .map(|_| client.watch_query::<GetAllSessions>(GetAllSessions {}))
        .collect();

    server_v1
        .ctx
        .apply_event_batch(vec![session_set_event("s-pre", "before-restart")])
        .expect("apply pre");

    for (i, cell) in cells.iter().enumerate() {
        let cell = cell.clone();
        wait_for(&format!("pre-restart on subscription #{i}"), move || {
            cell.get().iter().any(|s| s.id.0.as_ref() == "s-pre")
        });
    }

    server_v1.shutdown();
    let s2 = status.clone();
    wait_for("disconnect", move || {
        !matches!(s2.get(), ConnectionStatus::Connected(_))
    });

    let mut server_v2 = None;
    for _ in 0..30 {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| spawn_server(bind))) {
            Ok(s) => {
                server_v2 = Some(s);
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    let server_v2 = server_v2.expect("rebind");

    let s3 = status.clone();
    wait_for("reconnect", move || {
        matches!(s3.get(), ConnectionStatus::Connected(_))
    });
    thread::sleep(Duration::from_millis(500));

    server_v2
        .ctx
        .apply_event_batch(vec![session_set_event("s-post", "after-restart")])
        .expect("apply post");

    for (i, cell) in cells.iter().enumerate() {
        let cell = cell.clone();
        wait_for(&format!("post-restart on subscription #{i}"), move || {
            cell.get().iter().any(|s| s.id.0.as_ref() == "s-post")
        });
    }

    server_v2.shutdown();
}

/// Reproduces the leptos `live_query` lifecycle: subscribe a sink to
/// the `watch_query` cell, then drop the cell handle (let it go out
/// of scope). The sink should still receive updates because the
/// cell.own(guard) cycle keeps the cell alive.
#[test]
fn dropped_cell_handle_still_pumps_updates_after_reconnect() {
    let _ = env_logger::builder().is_test(true).try_init();
    entities::link();
    daemon::link();

    let port = pick_free_port();
    let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let addr = format!("ws://{bind}");

    let server_v1 = spawn_server(bind);

    let client = MykoClient::new();
    client.set_protocol(MykoProtocol::JSON);
    client.set_address(Some(addr.clone()));
    let status = client.connection_status();
    let s = status.clone();
    wait_for("initial connection", move || {
        matches!(s.get(), ConnectionStatus::Connected(_))
    });

    use std::sync::Mutex;
    let mirror: Arc<Mutex<Vec<Arc<Session>>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let cell = client.watch_query::<GetAllSessions>(GetAllSessions {});
        let mirror_for_sub = mirror.clone();
        let guard = <hyphae::Cell<Vec<Arc<Session>>, hyphae::CellImmutable> as hyphae::Watchable<
            Vec<Arc<Session>>,
        >>::subscribe(&cell, move |signal| {
            if let hyphae::Signal::Value(items) = signal {
                let mut m = mirror_for_sub.lock().unwrap();
                *m = (**items).to_vec();
            }
        });
        cell.own(guard);
    }

    server_v1
        .ctx
        .apply_event_batch(vec![session_set_event("s-pre", "before-restart")])
        .expect("apply pre-restart session");

    let m1 = mirror.clone();
    wait_for("pre-restart session in mirror sink", move || {
        m1.lock()
            .unwrap()
            .iter()
            .any(|s| s.id.0.as_ref() == "s-pre")
    });

    server_v1.shutdown();
    let s2 = status.clone();
    wait_for("client observes disconnect", move || {
        !matches!(s2.get(), ConnectionStatus::Connected(_))
    });

    let mut server_v2 = None;
    for _ in 0..30 {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| spawn_server(bind))) {
            Ok(s) => {
                server_v2 = Some(s);
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
    let server_v2 = server_v2.expect("rebind v2");

    let s3 = status.clone();
    wait_for("client reconnects", move || {
        matches!(s3.get(), ConnectionStatus::Connected(_))
    });

    thread::sleep(Duration::from_millis(500));

    server_v2
        .ctx
        .apply_event_batch(vec![session_set_event("s-post", "after-restart")])
        .expect("apply post-restart session");

    let m2 = mirror.clone();
    wait_for("post-restart session in mirror sink", move || {
        m2.lock()
            .unwrap()
            .iter()
            .any(|s| s.id.0.as_ref() == "s-post")
    });

    server_v2.shutdown();
}
