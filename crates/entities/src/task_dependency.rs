//! Server-owned dependencies between tasks, including across projects.

use myko::command::{CommandContext, CommandError, CommandHandler};
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};
use myko::{myko_command, myko_item};

use crate::TaskId;

#[myko_item]
pub struct TaskBlocker {
    pub task_id: TaskId,
    pub blocker_id: TaskId,
}

#[myko_command(TaskBlockerId)]
pub struct AddTaskDependency {
    pub task_id: TaskId,
    pub blocker_id: TaskId,
}

#[myko_command(TaskBlockerId)]
pub struct RemoveTaskDependency {
    pub task_id: TaskId,
    pub blocker_id: TaskId,
}

#[myko_command(Vec<TaskId>)]
pub struct ListReadyTasks {
    /// Omit to list ready tasks across all projects.
    pub project: Option<String>,
}

#[cfg(not(target_arch = "wasm32"))]
mod server {
    use super::*;
    use crate::{GetAllTaskClaims, GetAllTasks, Task, TaskClaim, TaskStatus};
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex, MutexGuard};

    // Serialize edits so two concurrent adds cannot both pass the cycle check.
    static TASK_EDITS: Mutex<()> = Mutex::new(());

    impl CommandHandler for AddTaskDependency {
        fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
            let _guard = lock_task_state(&ctx)?;
            let tasks: Vec<Arc<Task>> = ctx.exec_query(GetAllTasks {})?;
            for id in [&self.task_id, &self.blocker_id] {
                if !tasks.iter().any(|task| &task.id == id) {
                    return Err(task_error(&ctx, format!("Unknown task {id}")));
                }
            }

            let dependencies = ctx.exec_query(GetAllTaskBlockers {})?;
            let id = dependency_id(&self.task_id, &self.blocker_id);
            if dependencies.iter().any(|edge| edge.id == id) {
                return Ok(id);
            }
            let graph = dependency_graph(&dependencies);
            let mut pending = vec![&self.blocker_id];
            let mut seen = HashSet::new();
            while let Some(id) = pending.pop() {
                if id == &self.task_id {
                    return Err(task_error(&ctx, "Task dependency would create a cycle"));
                }
                if seen.insert(id) {
                    pending.extend(graph.get(id).into_iter().flatten().copied());
                }
            }

            ctx.emit_set(&TaskBlocker {
                id: id.clone(),
                task_id: self.task_id,
                blocker_id: self.blocker_id,
            })?;
            Ok(id)
        }
    }

    impl CommandHandler for RemoveTaskDependency {
        fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
            let _guard = lock_task_state(&ctx)?;
            let id = dependency_id(&self.task_id, &self.blocker_id);
            let dependencies: Vec<Arc<TaskBlocker>> = ctx.exec_query(GetAllTaskBlockers {})?;
            if let Some(edge) = dependencies.iter().find(|edge| edge.id == id) {
                ctx.emit_del(edge.as_ref())?;
            }
            Ok(id)
        }
    }

    impl CommandHandler for ListReadyTasks {
        fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
            let _guard = lock_task_state(&ctx)?;
            let claims = ctx.exec_query(GetAllTaskClaims {})?;
            let attempts = ctx.exec_query(crate::GetAllDispatchAttempts {})?;
            let reserved: HashSet<_> = attempts
                .iter()
                .filter(|attempt| {
                    !matches!(
                        attempt.state,
                        crate::DispatchState::Rejected { .. }
                            | crate::DispatchState::Claimed { .. }
                    )
                })
                .map(|attempt| &attempt.task_id)
                .collect();
            let mut ready = ready_tasks(
                &ctx,
                self.project.as_deref(),
                chrono::Utc::now().timestamp_millis(),
                &claims,
            )?;
            ready.retain(|id| !reserved.contains(id));
            Ok(ready)
        }
    }

    pub(crate) fn ready_tasks(
        ctx: &CommandContext,
        project: Option<&str>,
        now: i64,
        claims: &[Arc<TaskClaim>],
    ) -> Result<Vec<TaskId>, CommandError> {
        let tasks: Vec<Arc<Task>> = ctx.exec_query(GetAllTasks {})?;
        let dependencies = ctx.exec_query(GetAllTaskBlockers {})?;
        let closed: HashSet<_> = tasks
            .iter()
            .filter(|task| matches!(task.status, TaskStatus::Closed { .. }))
            .map(|task| &task.id)
            .collect();
        let claimed: HashSet<_> = claims
            .iter()
            .filter(|claim| claim.expires > now)
            .map(|claim| &claim.task_id)
            .collect();
        let graph = dependency_graph(&dependencies);
        let mut ready: Vec<_> = tasks
            .iter()
            .filter(|task| {
                task.status == TaskStatus::Open
                    && project.is_none_or(|project| task.project == project)
                    && !claimed.contains(&task.id)
                    && graph
                        .get(&task.id)
                        .into_iter()
                        .flatten()
                        .all(|id| closed.contains(id))
            })
            .collect();
        ready.sort_by(|left, right| {
            (left.priority, left.created, &left.id.0).cmp(&(
                right.priority,
                right.created,
                &right.id.0,
            ))
        });
        Ok(ready.into_iter().map(|task| task.id.clone()).collect())
    }

    fn dependency_graph(edges: &[Arc<TaskBlocker>]) -> HashMap<&TaskId, Vec<&TaskId>> {
        let mut graph = HashMap::<_, Vec<_>>::new();
        for edge in edges {
            graph
                .entry(&edge.task_id)
                .or_default()
                .push(&edge.blocker_id);
        }
        graph
    }

    fn dependency_id(task: &TaskId, blocker: &TaskId) -> TaskBlockerId {
        TaskBlockerId(Arc::from(format!("{task}/{blocker}")))
    }

    pub(crate) fn lock_task_state(
        ctx: &CommandContext,
    ) -> Result<MutexGuard<'static, ()>, CommandError> {
        TASK_EDITS
            .lock()
            .map_err(|_| task_error(ctx, "Task edits are unavailable"))
    }

    pub(crate) fn task_error(ctx: &CommandContext, message: impl Into<String>) -> CommandError {
        CommandError {
            tx: ctx.tx().to_string(),
            command_id: ctx.command_id.to_string(),
            message: message.into(),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use server::{lock_task_state, ready_tasks, task_error};

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;

    impl CommandHandler for AddTaskDependency {
        fn execute(self, _: CommandContext) -> Result<Self::Result, CommandError> {
            unreachable!("AddTaskDependency executes on the server")
        }
    }

    impl CommandHandler for RemoveTaskDependency {
        fn execute(self, _: CommandContext) -> Result<Self::Result, CommandError> {
            unreachable!("RemoveTaskDependency executes on the server")
        }
    }

    impl CommandHandler for ListReadyTasks {
        fn execute(self, _: CommandContext) -> Result<Self::Result, CommandError> {
            unreachable!("ListReadyTasks executes on the server")
        }
    }
}
