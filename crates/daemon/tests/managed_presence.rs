use marshal_entities::*;
use myko::{
    command::{CommandContext, CommandHandler},
    prelude::EventPublishing as _,
    request::RequestContext,
    server::{MykoServerContext, PersistError, Persister},
    wire::MEvent,
};
use myko_server::{BlackholePersister, MykoServer};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Durable(Mutex<Vec<MEvent>>);
impl Persister for Durable {
    fn persist(&self, event: MEvent) -> Result<(), PersistError> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

fn setup() -> (Arc<MykoServerContext>, Arc<Durable>) {
    marshal_entities::link();
    daemon::link();
    let durable = Arc::new(Durable::default());
    let server = MykoServer::builder()
        .with_default_persister(durable.clone())
        .with_persister_override("Client", Arc::new(BlackholePersister))
        .with_persister_override("Server", Arc::new(BlackholePersister))
        .with_persister_override("ManagedPresenceOwner", Arc::new(BlackholePersister))
        .build();
    let ctx = Arc::new(server.ctx());
    Box::leak(Box::new(server));
    (ctx, durable)
}

fn caller(ctx: &Arc<MykoServerContext>, client: Option<&str>) -> CommandContext {
    let req = if let Some(client) = client {
        RequestContext::from_client("presence-test".into(), Arc::from(client), ctx.host_id)
    } else {
        RequestContext::internal("presence-test".into(), ctx.host_id, "presence-test")
    };
    CommandContext::new("presence-test".into(), Arc::new(req), ctx.clone())
}

fn connect(ctx: &Arc<MykoServerContext>, id: &str) -> CommandContext {
    let command = caller(ctx, None);
    command
        .emit_set(&myko::entities::client::Client {
            id: myko::entities::client::ClientId(id.into()),
            server_id: myko::entities::server::ServerId("test-server".into()),
            address: None,
            windback: None,
        })
        .unwrap();
    caller(ctx, Some(id))
}

fn thread(id: &str) -> ManagedThreadRegistration {
    ManagedThreadRegistration {
        session_id: SessionId(id.into()),
        thread_id: id.into(),
        cwd: "/repo/Pulse".into(),
        title: Some("original title".into()),
        agent_id: Some(" launch-id ".into()),
        operator: Some("operator".into()),
        host: None,
    }
}

fn reconcile(
    ctx: &CommandContext,
    sequence: u64,
    threads: Vec<ManagedThreadRegistration>,
    retire: bool,
) -> Result<(), myko::command::CommandError> {
    ReconcileManagedPresence {
        sequence,
        threads,
        retire,
    }
    .execute(ctx.clone())
}

#[test]
fn fresh_observations_preserve_activity_delivery_metadata_and_flat_durable_writes() {
    let (ctx, durable) = setup();
    let owner = connect(&ctx, "owner");
    reconcile(&owner, 1, vec![thread("selected")], false).unwrap();
    let mut row = (*owner.exec_query(GetAllSessions {}).unwrap()[0]).clone();
    assert!(row.managed_presence);
    assert_eq!(row.project.as_deref(), Some("Pulse"));
    assert_eq!(row.agent_id.as_deref(), Some("launch-id"));
    row.client_id = Some(myko::entities::client::ClientId("delivery-client".into()));
    row.current_task = Some("in-flight task".into());
    row.session_name = Some("renamed title".into());
    row.last_activity_at = Some(123);
    row.pid = 42;
    owner.emit_set(&row).unwrap();
    let writes = durable.0.lock().unwrap().len();
    for sequence in 2..102 {
        reconcile(&owner, sequence, vec![thread("selected")], false).unwrap();
    }
    assert_eq!(
        durable.0.lock().unwrap().len(),
        writes,
        "unchanged renewals must not write durable events"
    );
    assert_eq!(*owner.exec_query(GetAllSessions {}).unwrap()[0], row);
    assert_eq!(
        owner
            .exec_query(GetAllManagedPresenceOwners {})
            .unwrap()
            .len(),
        1
    );
    owner.emit_del(&row).unwrap();
    reconcile(&owner, 102, vec![thread("selected")], false).unwrap();
    let repaired = &owner.exec_query(GetAllSessions {}).unwrap()[0];
    assert_eq!(repaired.id.0.as_ref(), "selected");
    assert_eq!(repaired.project.as_deref(), Some("Pulse"));
    assert_eq!(repaired.last_activity_at, None);
}

#[test]
fn retries_do_not_renew_and_retirement_fences_late_frames() {
    let (ctx, _) = setup();
    let owner = connect(&ctx, "owner");
    reconcile(&owner, 1, vec![thread("selected")], false).unwrap();
    let before = owner.exec_query(GetAllManagedPresenceOwners {}).unwrap()[0].observed_tick_ms;
    std::thread::sleep(std::time::Duration::from_millis(10));
    reconcile(&owner, 1, vec![thread("selected")], false).unwrap();
    assert_eq!(
        owner.exec_query(GetAllManagedPresenceOwners {}).unwrap()[0].observed_tick_ms,
        before
    );
    assert!(reconcile(&owner, 1, vec![thread("different")], false).is_err());
    assert!(reconcile(&owner, 0, vec![], false).is_err());
    reconcile(&owner, 2, vec![], true).unwrap();
    reconcile(&owner, 2, vec![], true).unwrap();
    assert!(reconcile(&owner, 3, vec![thread("selected")], false).is_err());
    let fresh = connect(&ctx, "new-owner");
    reconcile(&fresh, 1, vec![thread("selected")], false).unwrap();
    assert_eq!(
        fresh
            .exec_query(GetAllManagedPresenceOwners {})
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn two_observers_cannot_churn_existing_launch_correlation_or_durable_events() {
    let (ctx, durable) = setup();
    let first = connect(&ctx, "first-observer");
    let second = connect(&ctx, "second-observer");
    reconcile(&first, 1, vec![thread("selected")], false).unwrap();
    let writes = durable.0.lock().unwrap().len();
    let mut conflicting = thread("selected");
    conflicting.agent_id = Some("another-original-launch".into());
    for sequence in 2..12 {
        reconcile(&second, sequence, vec![conflicting.clone()], false).unwrap();
        assert_eq!(
            second.exec_query(GetAllSessions {}).unwrap()[0]
                .agent_id
                .as_deref(),
            Some("launch-id")
        );
        reconcile(&first, sequence, vec![thread("selected")], false).unwrap();
    }
    assert_eq!(durable.0.lock().unwrap().len(), writes);
}

#[test]
fn connection_console_and_snapshot_bounds_are_enforced() {
    let (ctx, _) = setup();
    assert!(reconcile(&caller(&ctx, None), 1, vec![thread("selected")], false).is_err());
    assert!(
        reconcile(
            &caller(&ctx, Some("not-live")),
            1,
            vec![thread("selected")],
            false
        )
        .is_err()
    );
    let owner = connect(&ctx, "owner");
    assert!(reconcile(&owner, 1, vec![thread("console-other-user:tab")], false).is_err());
    assert!(
        reconcile(
            &owner,
            1,
            vec![thread("selected"), thread("selected")],
            false
        )
        .is_err()
    );
    assert!(reconcile(&owner, 1, vec![thread("selected")], true).is_err());
    let mut oversized = thread("selected");
    oversized.cwd = "x".repeat(4097);
    assert!(reconcile(&owner, 1, vec![oversized], false).is_err());
    reconcile(&owner, 1, vec![], false).unwrap();
    assert_eq!(owner.exec_query(GetAllSessions {}).unwrap().len(), 0);
}

#[test]
fn hook_end_and_old_registration_cannot_remove_managed_policy() {
    let (ctx, _) = setup();
    let owner = connect(&ctx, "owner");
    reconcile(&owner, 1, vec![thread("selected")], false).unwrap();
    daemon::hooks::dispatch(
        "/hook/session-end",
        "",
        br#"{"session_id":"selected"}"#,
        &ctx,
    )
    .unwrap();
    let row = (*owner.exec_query(GetAllSessions {}).unwrap()[0]).clone();
    let mut legacy = row.clone();
    legacy.managed_presence = false;
    owner.emit_set(&legacy).unwrap();
    daemon::hooks::dispatch(
        "/hook/session-end",
        "",
        br#"{"session_id":"selected"}"#,
        &ctx,
    )
    .unwrap();
    assert_eq!(
        owner.exec_query(GetAllSessions {}).unwrap().len(),
        1,
        "a live claim must beat a legacy whole-row SET and delayed end"
    );
    assert_eq!(
        ReadManagedPresence {}
            .execute(owner.clone())
            .unwrap()
            .sessions,
        vec![SessionId("selected".into())]
    );
    reconcile(&owner, 2, vec![thread("selected")], false).unwrap();
    RegisterSession { session: legacy }
        .execute(owner.clone())
        .unwrap();
    assert!(owner.exec_query(GetAllSessions {}).unwrap()[0].managed_presence);
    assert_eq!(row.last_activity_at, None);
}

#[test]
fn canonical_promotion_and_concurrent_field_commands_preserve_each_others_updates() {
    let (ctx, _) = setup();
    let owner = connect(&ctx, "owner");
    let id = SessionId("selected".into());
    let mut original = new_hook_session(id.clone(), "/repo".into(), 1);
    original.session_name = Some("before promotion".into());
    owner.emit_set(&original).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let (done, completed) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        for field in 0..3 {
            let owner = owner.clone();
            let id = id.clone();
            let barrier = barrier.clone();
            let done = done.clone();
            scope.spawn(move || {
                barrier.wait();
                match field {
                    0 => SetSessionSessionName {
                        id,
                        session_name: Some("confirmed title".into()),
                    }
                    .execute(owner),
                    1 => SetSessionCurrentTask {
                        id,
                        current_task: Some("in-flight task".into()),
                    }
                    .execute(owner),
                    _ => SetSessionLastActivityAt {
                        id,
                        last_activity_at: Some(123),
                    }
                    .execute(owner),
                }
                .unwrap();
                done.send(()).unwrap();
            });
        }
        update_registered_session(&owner, &id, |prior| {
            let mut promoted = prior.unwrap().clone();
            barrier.wait();
            for _ in 0..3 {
                if completed
                    .recv_timeout(std::time::Duration::from_millis(150))
                    .is_err()
                {
                    break;
                }
            }
            promoted.managed_presence = true;
            promoted
        })
        .unwrap();
    });
    reconcile(&owner, 1, vec![thread("selected")], false).unwrap();
    let row = &owner.exec_query(GetAllSessions {}).unwrap()[0];
    assert!(row.managed_presence);
    assert_eq!(row.session_name.as_deref(), Some("confirmed title"));
    assert_eq!(row.current_task.as_deref(), Some("in-flight task"));
    assert_eq!(row.last_activity_at, Some(123));
}
