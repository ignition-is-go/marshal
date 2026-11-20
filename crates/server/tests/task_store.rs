use std::sync::Arc;

use hyphae::{Gettable, Materialize};
use marshal_entities::{
    AddTaskDependency, ClaimTask, CreateTask, DispatchAttempt, DispatchAttemptId, DispatchHarness,
    DispatchState, FindCoordination, ListReadyTasks, ReleaseTaskClaim, RemoveTaskDependency,
    RenewTaskClaim, Session, SessionId, SetTaskStatus, Task, TaskPriority, TaskStatus,
};
use myko::command::{CommandContext, CommandHandler};
use myko::{prelude::EventPublishing as _, request::RequestContext};
use myko_server::{BlackholePersister, MykoServer};

#[test]
fn creates_valid_tasks_in_the_server_registry() {
    marshal_entities::link();
    let server = MykoServer::builder()
        .with_default_persister(Arc::new(BlackholePersister))
        .build();
    let context = server.ctx();
    let command = |operation: &str| {
        let request = RequestContext::new(
            Arc::from("task-store-test"),
            None,
            vec![],
            uuid::Uuid::new_v4(),
            chrono::Utc::now().to_rfc3339(),
        );
        CommandContext::new(
            Arc::from(operation),
            Arc::new(request),
            Arc::new(context.clone()),
        )
    };
    let create = |title: &str| {
        CreateTask {
            project: " marshal ".into(),
            title: title.into(),
            body: String::new(),
            priority: TaskPriority::P1,
            labels: vec!["dispatch".into()],
        }
        .execute(command("CreateTask"))
    };

    assert!(create(" ").is_err());
    let id = create(" Add remote workers ").unwrap();
    let entries = context
        .registry
        .get("Task")
        .unwrap()
        .entries()
        .materialize()
        .get();
    assert_eq!(entries.len(), 1);
    let task = entries
        .into_iter()
        .find_map(|(_, item)| myko::utils::downcast_item::<Task>(&item))
        .unwrap();
    assert_eq!(task.id, id);
    assert_eq!(task.title, "Add remote workers");
    assert_eq!(task.project, "marshal");
    assert_eq!(task.priority, TaskPriority::P1);
    assert_eq!(task.status, TaskStatus::Open);

    let blocker = create("Prepare a host").unwrap();
    let edge = AddTaskDependency {
        task_id: id.clone(),
        blocker_id: blocker.clone(),
    };
    let edge_id = edge.execute(command("AddTaskDependency")).unwrap();
    assert_eq!(
        AddTaskDependency {
            task_id: id.clone(),
            blocker_id: blocker.clone()
        }
        .execute(command("AddTaskDependency"))
        .unwrap(),
        edge_id
    );
    assert!(
        AddTaskDependency {
            task_id: blocker.clone(),
            blocker_id: id.clone()
        }
        .execute(command("AddTaskDependency"))
        .is_err()
    );

    let ready = || {
        ListReadyTasks {
            project: Some("marshal".into()),
        }
        .execute(command("ListReadyTasks"))
        .unwrap()
    };
    assert_eq!(ready(), vec![blocker.clone()]);
    SetTaskStatus {
        id: blocker.clone(),
        status: TaskStatus::Closed { commit: None },
    }
    .execute(command("SetTaskStatus"))
    .unwrap();
    assert_eq!(ready(), vec![id.clone()]);
    SetTaskStatus {
        id: blocker.clone(),
        status: TaskStatus::Open,
    }
    .execute(command("SetTaskStatus"))
    .unwrap();
    assert_eq!(ready(), vec![blocker.clone()]);
    RemoveTaskDependency {
        task_id: id.clone(),
        blocker_id: blocker.clone(),
    }
    .execute(command("RemoveTaskDependency"))
    .unwrap();
    assert_eq!(ready(), vec![id.clone(), blocker.clone()]);
    let claim = |session| {
        ClaimTask {
            task_id: id.clone(),
            session_id: SessionId(Arc::from(session)),
        }
        .execute(command("ClaimTask"))
    };
    let mut dispatch = DispatchAttempt {
        id: DispatchAttemptId(id.0.clone()),
        task_id: id.clone(),
        launch_id: "reserved-launch".into(),
        host_id: "host".into(),
        harness: DispatchHarness::Codex,
        workspace: "session".into(),
        state: DispatchState::Reserved,
        created: 1,
    };
    command("SeedDispatch").emit_set(&dispatch).unwrap();
    assert!(claim("worker-1").is_err());
    dispatch.state = DispatchState::Rejected {
        reason: "test rejection".into(),
    };
    command("SeedDispatch").emit_set(&dispatch).unwrap();
    let first = claim("worker-1").unwrap();
    assert_eq!(claim("worker-1").unwrap().lease_id, first.lease_id);
    assert!(claim("worker-2").is_err());
    assert_eq!(ready(), vec![blocker.clone()]);

    let idle: Session = serde_json::from_value(serde_json::json!({
        "id": "available",
        "pid": 0,
        "cwd": "/work/marshal",
        "connectedAt": 1,
        "activity": "idle",
        "project": "marshal",
        "repository": "github.com/team/marshal"
    }))
    .unwrap();
    command("test-setup").emit_set(&idle).unwrap();
    let mut same_name = idle.clone();
    same_name.id = SessionId(Arc::from("different-origin"));
    same_name.repository = Some("github.com/another-team/marshal".into());
    command("test-setup").emit_set(&same_name).unwrap();
    let matches = FindCoordination {
        project: Some("github.com/team/marshal".into()),
        query: Some("REMOTE workers".into()),
    }
    .execute(command("FindCoordination"))
    .unwrap();
    assert_eq!(matches.tasks.len(), 1);
    assert_eq!(matches.tasks[0].id, id);
    assert_eq!(
        matches.tasks[0].owner_session_id.as_ref(),
        Some(&first.session_id)
    );
    assert_eq!(matches.idle_agents.len(), 1);
    assert_eq!(matches.idle_agents[0].session_id, idle.id);

    let renew = RenewTaskClaim {
        task_id: id.clone(),
        lease_id: first.lease_id.clone(),
    }
    .execute(command("RenewTaskClaim"))
    .unwrap();
    assert_eq!(renew.session_id, first.session_id);
    assert!(renew.expires >= first.expires);

    let release = || {
        ReleaseTaskClaim {
            task_id: id.clone(),
            lease_id: first.lease_id.clone(),
        }
        .execute(command("ReleaseTaskClaim"))
    };
    release().unwrap();
    assert_eq!(ready(), vec![id.clone(), blocker.clone()]);
    let second = claim("worker-2").unwrap();
    assert_ne!(second.lease_id, first.lease_id);
    assert!(release().is_err());
    assert_eq!(ready(), vec![blocker]);
}
