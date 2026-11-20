//! One renewable server-side lease per task.

use myko::command::{CommandContext, CommandError, CommandHandler};
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};
use myko::{myko_command, myko_item};

use crate::{SessionId, TaskId};

#[myko_item]
pub struct TaskClaim {
    pub task_id: TaskId,
    pub session_id: SessionId,
    /// An individual claim attempt, used to reject stale renewals and releases.
    pub lease_id: String,
    /// Wall-clock expiry in Unix milliseconds.
    pub expires: i64,
}

#[myko_command(TaskClaim)]
pub struct ClaimTask {
    pub task_id: TaskId,
    pub session_id: SessionId,
}

#[myko_command(TaskClaim)]
pub struct RenewTaskClaim {
    pub task_id: TaskId,
    pub lease_id: String,
}

#[myko_command]
pub struct ReleaseTaskClaim {
    pub task_id: TaskId,
    pub lease_id: String,
}

#[cfg(not(target_arch = "wasm32"))]
mod server {
    use super::*;
    use crate::task_dependency::{lock_task_state, ready_tasks, task_error};
    use std::sync::Arc;

    const CLAIM_TTL_MILLIS: i64 = 24 * 60 * 60 * 1000;

    impl CommandHandler for ClaimTask {
        fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
            let _guard = lock_task_state(&ctx)?;
            claim_ready_task(&ctx, self.task_id, self.session_id)
        }
    }

    pub(crate) fn claim_ready_task(
        ctx: &CommandContext,
        task_id: TaskId,
        session_id: SessionId,
    ) -> Result<TaskClaim, CommandError> {
        let now = chrono::Utc::now().timestamp_millis();
        let claims = ctx.exec_query(GetAllTaskClaims {})?;
        if let Some(claim) = claims
            .iter()
            .find(|claim| claim.task_id == task_id && claim.expires > now)
        {
            if claim.session_id == session_id {
                return Ok(claim.as_ref().clone());
            }
            return Err(task_error(ctx, "Task is already claimed"));
        }
        if session_id.0.trim().is_empty() {
            return Err(task_error(ctx, "A claim requires a session ID"));
        }
        if !ready_tasks(ctx, None, now, &claims)?.contains(&task_id) {
            return Err(task_error(ctx, "Task is missing, closed, or blocked"));
        }

        let claim = TaskClaim {
            id: TaskClaimId(task_id.0.clone()),
            task_id,
            session_id,
            lease_id: uuid::Uuid::now_v7().to_string(),
            expires: now + CLAIM_TTL_MILLIS,
        };
        ctx.emit_set(&claim)?;
        Ok(claim)
    }

    impl CommandHandler for RenewTaskClaim {
        fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
            let _guard = lock_task_state(&ctx)?;
            let now = chrono::Utc::now().timestamp_millis();
            let mut claim = find_claim(&ctx, &self.task_id, &self.lease_id)?
                .filter(|claim| claim.expires > now)
                .ok_or_else(|| task_error(&ctx, "Task claim is missing or expired"))?
                .as_ref()
                .clone();
            claim.expires = now + CLAIM_TTL_MILLIS;
            ctx.emit_set(&claim)?;
            Ok(claim)
        }
    }

    impl CommandHandler for ReleaseTaskClaim {
        fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
            let _guard = lock_task_state(&ctx)?;
            if let Some(claim) = find_claim(&ctx, &self.task_id, &self.lease_id)? {
                ctx.emit_del(claim.as_ref())?;
            }
            Ok(())
        }
    }

    fn find_claim(
        ctx: &CommandContext,
        task: &TaskId,
        lease: &str,
    ) -> Result<Option<Arc<TaskClaim>>, CommandError> {
        let claims: Vec<Arc<TaskClaim>> = ctx.exec_query(GetAllTaskClaims {})?;
        match claims.into_iter().find(|claim| &claim.task_id == task) {
            Some(claim) if claim.lease_id == lease => Ok(Some(claim)),
            Some(_) => Err(task_error(ctx, "Task claim has a different lease ID")),
            None => Ok(None),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use server::claim_ready_task;

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;

    impl CommandHandler for ClaimTask {
        fn execute(self, _: CommandContext) -> Result<Self::Result, CommandError> {
            unreachable!("ClaimTask executes on the server")
        }
    }

    impl CommandHandler for RenewTaskClaim {
        fn execute(self, _: CommandContext) -> Result<Self::Result, CommandError> {
            unreachable!("RenewTaskClaim executes on the server")
        }
    }

    impl CommandHandler for ReleaseTaskClaim {
        fn execute(self, _: CommandContext) -> Result<Self::Result, CommandError> {
            unreachable!("ReleaseTaskClaim executes on the server")
        }
    }
}
