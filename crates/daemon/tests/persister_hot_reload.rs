//! Integration tests for the disk persister's hot-reload watcher.
//!
//! Spins up a real `CellServer` against a temp `events.jsonl`, exercises
//! the watcher's three contracts:
//!   1. External appends are picked up live.
//!   2. Truncation / replacement reloads from the top.
//!   3. The daemon's own appends don't double-apply.

use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use daemon::persister::DiskPersister;
use entities::Session;
use hyphae::Gettable;
use myko::{
    core::item::Eventable,
    server::{CellServerCtx, Persister},
    wire::{MEvent, MEventType},
};
use myko_server::CellServer;

/// Polling interval is 150ms in start_watcher; allow a generous slack
/// for filesystem-event delivery + apply.
const POLL_TIMEOUT: Duration = Duration::from_secs(3);

fn setup() -> (
    PathBuf,
    Arc<DiskPersister>,
    CellServerCtx,
    tempfile::TempDir,
) {
    entities::link();
    daemon::link();

    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("events.jsonl");
    let persister = Arc::new(DiskPersister::new(&log_path).expect("open log"));

    let server = CellServer::builder()
        .with_default_persister(persister.clone() as Arc<dyn Persister>)
        .build();
    let ctx = server.ctx();
    persister.replay(&ctx).expect("replay empty log");

    let server: &'static CellServer = Box::leak(Box::new(server));
    let _ = server;

    (log_path, persister, ctx, dir)
}

fn session_set_line(id: &str, nickname: &str) -> String {
    let event = MEvent {
        item: serde_json::json!({
            "id": id,
            "nickname": nickname,
            "pid": 0,
            "cwd": "/tmp",
            "connectedAt": chrono::Utc::now().timestamp_millis(),
        }),
        change_type: MEventType::SET,
        item_type: "Session".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        tx: uuid::Uuid::new_v4().to_string(),
        source_id: Some("test".to_string()),
        options: None,
    };
    serde_json::to_string(&event).expect("serialize event")
}

fn append_external(path: &Path, line: &str) {
    let mut f = OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open log for external append");
    writeln!(f, "{}", line).expect("append line");
    f.sync_data().expect("fsync");
}

fn session_count(ctx: &CellServerCtx) -> usize {
    ctx.registry
        .get(Session::ENTITY_NAME_STATIC)
        .map(|s| s.entries().get().len())
        .unwrap_or(0)
}

fn wait_for_sessions(ctx: &CellServerCtx, expected: usize) -> usize {
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let n = session_count(ctx);
        if n >= expected || Instant::now() >= deadline {
            return n;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn external_append_is_picked_up() {
    let (log_path, persister, ctx, _dir) = setup();
    let _watcher = persister.start_watcher(ctx.clone()).expect("start watcher");

    assert_eq!(session_count(&ctx), 0, "registry starts empty");

    append_external(&log_path, &session_set_line("s-ext-1", "external"));
    let n = wait_for_sessions(&ctx, 1);
    assert_eq!(
        n, 1,
        "external append should be applied within {POLL_TIMEOUT:?}"
    );
}

#[test]
fn truncation_triggers_reload() {
    let (log_path, persister, ctx, _dir) = setup();
    let _watcher = persister.start_watcher(ctx.clone()).expect("start watcher");

    for i in 0..3 {
        append_external(
            &log_path,
            &session_set_line(&format!("s-pre-{i}"), "before"),
        );
    }
    assert_eq!(wait_for_sessions(&ctx, 3), 3);

    {
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&log_path)
            .expect("truncate log");
        writeln!(f, "{}", session_set_line("s-post-1", "after")).unwrap();
        f.sync_data().unwrap();
    }

    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let has_post = ctx
            .registry
            .get(Session::ENTITY_NAME_STATIC)
            .map(|s| {
                s.entries()
                    .get()
                    .iter()
                    .any(|(id, _)| id.as_ref() == "s-post-1")
            })
            .unwrap_or(false);
        if has_post {
            break;
        }
        if Instant::now() >= deadline {
            panic!("post-truncation event not applied within {POLL_TIMEOUT:?}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn rename_replacement_triggers_reload() {
    let (log_path, persister, ctx, dir) = setup();
    let _watcher = persister.start_watcher(ctx.clone()).expect("start watcher");

    append_external(&log_path, &session_set_line("s-orig-1", "original"));
    assert_eq!(wait_for_sessions(&ctx, 1), 1);

    let staging = dir.path().join("events.jsonl.new");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&staging)
            .expect("open staging");
        writeln!(f, "{}", session_set_line("s-replaced-1", "via mv")).unwrap();
        f.sync_data().unwrap();
    }
    std::fs::rename(&staging, &log_path).expect("rename onto log");

    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let has_replaced = ctx
            .registry
            .get(Session::ENTITY_NAME_STATIC)
            .map(|s| {
                s.entries()
                    .get()
                    .iter()
                    .any(|(id, _)| id.as_ref() == "s-replaced-1")
            })
            .unwrap_or(false);
        if has_replaced {
            break;
        }
        if Instant::now() >= deadline {
            panic!("post-rename event not applied within {POLL_TIMEOUT:?}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn own_writes_do_not_double_apply() {
    let (log_path, persister, ctx, _dir) = setup();
    let _watcher = persister.start_watcher(ctx.clone()).expect("start watcher");

    let line = session_set_line("s-own-1", "via apply_event_batch");
    let event: MEvent = serde_json::from_str(&line).unwrap();
    let applied = ctx.apply_event_batch(vec![event]).expect("apply own event");
    assert_eq!(applied, 1);
    assert_eq!(session_count(&ctx), 1, "registry has the one session");

    thread::sleep(Duration::from_millis(500));

    let on_disk = std::fs::read_to_string(&log_path).unwrap();
    let line_count = on_disk.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(
        line_count, 1,
        "exactly one event line on disk; got:\n{on_disk}"
    );
    assert_eq!(
        session_count(&ctx),
        1,
        "registry still has exactly one session after watcher ticks",
    );

    append_external(
        &log_path,
        &session_set_line("s-own-2", "external after own"),
    );
    assert_eq!(wait_for_sessions(&ctx, 2), 2);
}
