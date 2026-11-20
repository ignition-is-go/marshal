//! A persisted reservation to staff a task through an opted-in host.

use myko::command::{CommandContext, CommandError, CommandHandler};
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};
use myko::{myko_command, myko_item, myko_subtype};

use crate::{SessionId, TaskId};

pub const DISPATCH_SENDER: &str = "console-marshal-dispatch:server";

#[myko_subtype(derive(Copy, Eq))]
pub enum DispatchHarness {
    Claude,
    Codex,
}

#[myko_subtype(derive(Eq))]
pub enum DispatchState {
    Reserved,
    Starting,
    Running {
        handle: String,
    },
    Joined {
        handle: String,
        session_id: SessionId,
    },
    Offered {
        handle: String,
        session_id: SessionId,
    },
    Delivered {
        handle: String,
        session_id: SessionId,
    },
    Claimed {
        handle: String,
        session_id: SessionId,
        lease_id: String,
    },
    Rejected {
        reason: String,
    },
}

#[myko_item]
pub struct DispatchAttempt {
    pub task_id: TaskId,
    pub launch_id: String,
    pub host_id: String,
    pub harness: DispatchHarness,
    pub workspace: String,
    pub state: DispatchState,
    pub created: i64,
}

#[myko_command(DispatchAttempt)]
pub struct ReserveTaskDispatch {
    pub task_id: TaskId,
    pub host_id: String,
    pub harness: DispatchHarness,
    pub workspace: String,
}

impl CommandHandler for ReserveTaskDispatch {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("ReserveTaskDispatch executes on the server")
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        use crate::GetAllTaskClaims;
        use crate::task_dependency::{lock_task_state, ready_tasks, task_error};

        if ctx
            .req
            .auth
            .as_ref()
            .and_then(|auth| auth.identity())
            .and_then(|identity| identity.user_id.as_ref())
            .is_none()
        {
            return Err(task_error(
                &ctx,
                "Dispatch requires an authenticated operator",
            ));
        }
        let _guard = lock_task_state(&ctx)?;
        let now = chrono::Utc::now().timestamp_millis();
        let claims = ctx.exec_query(GetAllTaskClaims {})?;
        let attempts = ctx.exec_query(GetAllDispatchAttempts {})?;
        if let Some(existing) = attempts
            .iter()
            .filter(|attempt| attempt.task_id == self.task_id)
            .find(|attempt| match &attempt.state {
                DispatchState::Rejected { .. } => false,
                DispatchState::Claimed { lease_id, .. } => claims.iter().any(|claim| {
                    claim.task_id == self.task_id
                        && claim.lease_id == *lease_id
                        && claim.expires > now
                }),
                _ => true,
            })
        {
            if existing.host_id == self.host_id
                && existing.harness == self.harness
                && existing.workspace == self.workspace
            {
                return Ok(existing.as_ref().clone());
            }
            return Err(task_error(&ctx, "Task already has a dispatch attempt"));
        }

        if self.host_id.is_empty()
            || self.host_id.len() > 128
            || self.workspace.trim().is_empty()
            || self.workspace.len() > 4096
            || !self
                .host_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        {
            return Err(task_error(
                &ctx,
                "Dispatch requires a valid host ID and workspace",
            ));
        }

        if !ready_tasks(&ctx, None, now, &claims)?.contains(&self.task_id) {
            return Err(task_error(
                &ctx,
                "Task is missing, closed, claimed, or blocked",
            ));
        }

        let attempt = DispatchAttempt {
            id: DispatchAttemptId(self.task_id.0.clone()),
            task_id: self.task_id,
            launch_id: uuid::Uuid::now_v7().to_string(),
            host_id: self.host_id,
            harness: self.harness,
            workspace: self.workspace,
            state: DispatchState::Reserved,
            created: now,
        };
        ctx.emit_set(&attempt)?;
        Ok(attempt)
    }
}

/// Recheck eligibility and record that a start may have reached the host.
#[cfg(not(target_arch = "wasm32"))]
pub fn begin_dispatch(ctx: &CommandContext, id: &DispatchAttemptId) -> Result<bool, CommandError> {
    use crate::GetAllTaskClaims;
    use crate::task_dependency::{lock_task_state, ready_tasks};

    let _guard = lock_task_state(ctx)?;
    let Some(attempt) = ctx.exec_report(GetDispatchAttemptById { id: id.clone() })? else {
        return Ok(false);
    };
    if attempt.state != DispatchState::Reserved {
        return Ok(false);
    }
    let now = chrono::Utc::now().timestamp_millis();
    let claims = ctx.exec_query(GetAllTaskClaims {})?;
    let ready = ready_tasks(ctx, None, now, &claims)?.contains(&attempt.task_id);
    let mut updated = attempt.as_ref().clone();
    updated.state = if ready {
        DispatchState::Starting
    } else {
        DispatchState::Rejected {
            reason: "Task is no longer ready".into(),
        }
    };
    ctx.emit_set(&updated)?;
    Ok(ready)
}

/// Store one assignment in the joined session's inbox. The message key is
/// derived from the launch ID so reprocessing a joined row can safely resume.
#[cfg(not(target_arch = "wasm32"))]
pub fn offer_dispatched_task(
    ctx: &CommandContext,
    id: &DispatchAttemptId,
) -> Result<(), CommandError> {
    use crate::task_dependency::{lock_task_state, ready_tasks};
    use crate::{
        GetAllTaskClaims, GetMessageById, GetSessionById, GetTaskById, Message, MessageId,
    };
    use std::sync::Arc;

    let _guard = lock_task_state(ctx)?;
    let Some(attempt) = ctx.exec_report(GetDispatchAttemptById { id: id.clone() })? else {
        return Ok(());
    };
    let DispatchState::Joined { handle, session_id } = &attempt.state else {
        return Ok(());
    };
    let now = chrono::Utc::now().timestamp_millis();
    let claims = ctx.exec_query(GetAllTaskClaims {})?;
    if !ready_tasks(ctx, None, now, &claims)?.contains(&attempt.task_id) {
        return Ok(());
    }
    let session = ctx.exec_report(GetSessionById {
        id: session_id.clone(),
    })?;
    if session
        .as_ref()
        .and_then(|session| session.agent_id.as_deref())
        != Some(attempt.launch_id.as_str())
    {
        return Ok(());
    }
    let Some(task) = ctx.exec_report(GetTaskById {
        id: attempt.task_id.clone(),
    })?
    else {
        return Ok(());
    };

    let message_id = MessageId(Arc::from(format!("dispatch:{}", attempt.launch_id)));
    if ctx
        .exec_report(GetMessageById {
            id: message_id.clone(),
        })?
        .is_none()
    {
        ctx.emit_set(&Message {
            id: message_id,
            from_session_id: SessionId(Arc::from(DISPATCH_SENDER)),
            to_session_id: Some(session_id.clone()),
            to_room_id: None,
            to_operator: None,
            response_required: false,
            body: format!(
                "Marshal task assignment {} in {}\n{}\n{}\nBefore starting work, accept this assignment with marshal task accept {}",
                task.id, task.project, task.title, task.body, task.id
            ),
            sent_at: now,
        })?;
    }
    let mut updated = attempt.as_ref().clone();
    updated.state = DispatchState::Offered {
        handle: handle.clone(),
        session_id: session_id.clone(),
    };
    ctx.emit_set(&updated)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn observe_dispatch_delivery(
    ctx: &CommandContext,
    id: &DispatchAttemptId,
) -> Result<(), CommandError> {
    use crate::task_dependency::lock_task_state;
    use crate::{GetMessageReadById, MessageRead, MessageReadId};
    use std::sync::Arc;

    let _guard = lock_task_state(ctx)?;
    let Some(attempt) = ctx.exec_report(GetDispatchAttemptById { id: id.clone() })? else {
        return Ok(());
    };
    let DispatchState::Offered { handle, session_id } = &attempt.state else {
        return Ok(());
    };
    let message_id = format!("dispatch:{}", attempt.launch_id);
    let id = MessageReadId(Arc::from(MessageRead::make_id(
        &message_id,
        session_id.0.as_ref(),
    )));
    if ctx.exec_report(GetMessageReadById { id })?.is_some() {
        let mut updated = attempt.as_ref().clone();
        updated.state = DispatchState::Delivered {
            handle: handle.clone(),
            session_id: session_id.clone(),
        };
        ctx.emit_set(&updated)?;
    }
    Ok(())
}

/// Accept only the assignment previously bound to the reporting host's session.
#[cfg(not(target_arch = "wasm32"))]
pub fn accept_dispatched_task(
    ctx: &CommandContext,
    id: &DispatchAttemptId,
    launch_id: &str,
    session: &SessionId,
    task_id: &str,
) -> Result<bool, CommandError> {
    use crate::GetSessionById;
    use crate::task_claim::claim_ready_task;
    use crate::task_dependency::lock_task_state;

    let _guard = lock_task_state(ctx)?;
    let Some(attempt) = ctx.exec_report(GetDispatchAttemptById { id: id.clone() })? else {
        return Ok(false);
    };
    if let DispatchState::Claimed { session_id, .. } = &attempt.state {
        return Ok(attempt.launch_id == launch_id
            && attempt.task_id.0.as_ref() == task_id
            && session_id == session);
    }
    let (DispatchState::Offered { handle, session_id }
    | DispatchState::Delivered { handle, session_id }) = &attempt.state
    else {
        return Ok(false);
    };
    if attempt.launch_id != launch_id
        || attempt.task_id.0.as_ref() != task_id
        || session_id != session
    {
        return Ok(false);
    }
    let registered = ctx.exec_report(GetSessionById {
        id: session_id.clone(),
    })?;
    if registered
        .as_ref()
        .and_then(|session| session.agent_id.as_deref())
        != Some(launch_id)
    {
        return Ok(false);
    }
    let claim = claim_ready_task(ctx, attempt.task_id.clone(), session_id.clone())?;
    let mut updated = attempt.as_ref().clone();
    updated.state = DispatchState::Claimed {
        handle: handle.clone(),
        session_id: session_id.clone(),
        lease_id: claim.lease_id,
    };
    ctx.emit_set(&updated)?;
    Ok(true)
}

/// Accept a dispatch using the launch-scoped proof supplied by its host.
#[myko_command]
#[myko_public]
pub struct AcceptTaskAssignment {
    pub task_id: TaskId,
    pub launch_id: String,
    pub session_id: SessionId,
    pub proof: String,
}

#[cfg(not(target_arch = "wasm32"))]
static ASSIGNMENT_CREDENTIALS: std::sync::OnceLock<std::collections::HashMap<String, String>> =
    std::sync::OnceLock::new();

#[cfg(not(target_arch = "wasm32"))]
pub fn configure_assignment_credentials(
    credentials: std::collections::HashMap<String, String>,
) -> Result<(), AlreadyConfigured> {
    ASSIGNMENT_CREDENTIALS
        .set(credentials)
        .map_err(|_| AlreadyConfigured)
}

#[cfg(not(target_arch = "wasm32"))]
pub struct AlreadyConfigured;

/// Derive the proof a trusted host may hand to one guest.
#[cfg(not(target_arch = "wasm32"))]
pub fn assignment_proof(host_token: &str, launch_id: &str) -> String {
    use hmac::{Hmac, Mac as _};

    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(host_token.as_bytes())
        .expect("HMAC accepts keys of any size");
    mac.update(b"marshal:assignment-acceptance:v1:");
    mac.update(launch_id.as_bytes());
    format!("{:x}", mac.finalize().into_bytes())
}

impl CommandHandler for AcceptTaskAssignment {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("AcceptTaskAssignment executes on the server")
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        use subtle::ConstantTimeEq as _;

        let denied =
            || crate::task_dependency::task_error(&ctx, "Assignment could not be accepted");
        if self.proof.len() != 64
            || uuid::Uuid::parse_str(self.task_id.0.as_ref()).is_err()
            || uuid::Uuid::parse_str(&self.launch_id).is_err()
            || self.session_id.0.is_empty()
            || self.session_id.0.len() > 256
        {
            return Err(denied());
        }
        let id = DispatchAttemptId(self.task_id.0.clone());
        let attempt = ctx
            .exec_report(GetDispatchAttemptById { id: id.clone() })?
            .filter(|attempt| attempt.launch_id == self.launch_id)
            .ok_or_else(denied)?;
        let key = ASSIGNMENT_CREDENTIALS
            .get()
            .and_then(|credentials| credentials.get(&attempt.host_id))
            .ok_or_else(denied)?;
        let expected = assignment_proof(key, &self.launch_id);
        if !bool::from(expected.as_bytes().ct_eq(self.proof.as_bytes())) {
            return Err(denied());
        }
        if !accept_dispatched_task(
            &ctx,
            &id,
            &self.launch_id,
            &self.session_id,
            self.task_id.0.as_ref(),
        )? {
            return Err(denied());
        }
        Ok(())
    }
}
