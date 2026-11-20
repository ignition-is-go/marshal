//! Drives persisted reservations through live host connections.

use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::{StreamExt as _, stream};

use marshal_client::host::{ExecutionRequest, ExecutionResult, Harness, HostRegistration};
use marshal_entities::{
    DispatchAttempt, DispatchHarness, DispatchState, GetAllDispatchAttempts, GetAllSessions,
    GetSessionById, SessionId, accept_dispatched_task, begin_dispatch, observe_dispatch_delivery,
    offer_dispatched_task, reject_reserved_dispatch,
};
use myko::{
    command::{CommandContext, CommandError},
    prelude::EventPublishing as _,
    request::RequestContext,
    server::MykoServerContext,
};

use crate::hosts::HostRegistry;

pub async fn run(context: Arc<MykoServerContext>, hosts: HostRegistry) {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(error) = reconcile_once(&context, &hosts).await {
            log::warn!("task dispatch reconciliation failed: {error:?}");
        }
    }
}

pub async fn reconcile_once(
    context: &Arc<MykoServerContext>,
    hosts: &HostRegistry,
) -> Result<(), CommandError> {
    let req = RequestContext::internal(
        uuid::Uuid::new_v4().to_string().into(),
        context.host_id,
        "dispatch",
    );
    let command = CommandContext::new(Arc::from("dispatch"), Arc::new(req), context.clone());
    let attempts: Vec<Arc<DispatchAttempt>> = command.exec_query(GetAllDispatchAttempts {})?;
    let connected = hosts.snapshot().await;
    let mut by_host: HashMap<&str, Vec<&DispatchAttempt>> = HashMap::new();
    for attempt in &attempts {
        by_host.entry(&attempt.host_id).or_default().push(attempt);
    }
    let rounds: Vec<_> = by_host
        .into_iter()
        .map(|(host_id, attempts)| {
            reconcile_host(&command, hosts, connected.get(host_id), attempts)
        })
        .collect();
    let mut rounds = stream::iter(rounds).buffer_unordered(16);
    let mut first_error = None;
    while let Some(result) = rounds.next().await {
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

async fn reconcile_host(
    command: &CommandContext,
    hosts: &HostRegistry,
    host: Option<&HostRegistration>,
    attempts: Vec<&DispatchAttempt>,
) -> Result<(), CommandError> {
    for attempt in attempts {
        if matches!(attempt.state, DispatchState::Joined { .. }) {
            offer_dispatched_task(command, &attempt.id)?;
            continue;
        }
        if matches!(attempt.state, DispatchState::Offered { .. }) {
            observe_dispatch_delivery(command, &attempt.id)?;
        }
        let Some(host) = host else {
            continue;
        };
        let harness = match attempt.harness {
            DispatchHarness::Claude => Harness::Claude,
            DispatchHarness::Codex => Harness::Codex,
        };
        let request = match attempt.state {
            DispatchState::Reserved => {
                if !host.launch_harnesses.contains(&harness) {
                    reject_reserved_dispatch(
                        command,
                        &attempt.id,
                        &attempt.launch_id,
                        "Host has not opted into this harness".into(),
                    )?;
                    continue;
                }
                if !begin_dispatch(command, &attempt.id)? {
                    continue;
                }
                ExecutionRequest::Start {
                    launch_id: attempt.launch_id.clone(),
                    harness,
                    workspace: attempt.workspace.clone(),
                }
            }
            DispatchState::Starting
            | DispatchState::Running { .. }
            | DispatchState::Offered { .. }
            | DispatchState::Delivered { .. } => ExecutionRequest::Inspect {
                launch_id: attempt.launch_id.clone(),
            },
            _ => continue,
        };
        let starting = matches!(request, ExecutionRequest::Start { .. });
        let outcome = hosts.execute(&attempt.host_id, request).await;
        let awaiting_acceptance = matches!(
            attempt.state,
            DispatchState::Offered { .. } | DispatchState::Delivered { .. }
        );
        let outcome = match outcome {
            Ok(ExecutionResult::Accepted {
                session_id,
                task_id,
                ..
            }) if awaiting_acceptance => {
                let session_id = SessionId(Arc::from(session_id));
                if let Err(error) = accept_dispatched_task(
                    command,
                    &attempt.id,
                    &attempt.launch_id,
                    &session_id,
                    &task_id,
                ) {
                    log::debug!("dispatch acceptance could not be applied: {error:?}");
                }
                continue;
            }
            Ok(ExecutionResult::Accepted {
                handle, session_id, ..
            }) => Ok(ExecutionResult::Attached { handle, session_id }),
            other => other,
        };
        if awaiting_acceptance {
            continue;
        }
        match outcome {
            Ok(ExecutionResult::Running { handle }) => {
                if attempt.state
                    != (DispatchState::Running {
                        handle: handle.clone(),
                    })
                {
                    set_state(command, attempt, DispatchState::Running { handle })?;
                }
            }
            Ok(ExecutionResult::Managed { handle, host, cwd }) => {
                // The managed host attests the guest hostname and workspace. Require one
                // roster session with all three facts before offering work.
                let sessions = command.exec_query(GetAllSessions {})?;
                let mut matching = sessions.iter().filter(|session| {
                    session.agent_id.as_deref() == Some(attempt.launch_id.as_str())
                        && session.cwd == cwd
                        && session.host.as_ref().is_some_and(|session_host| {
                            session_host
                                .name
                                .eq_ignore_ascii_case(host.split('.').next().unwrap_or(&host))
                        })
                });
                if let Some(session) = matching.next()
                    && matching.next().is_none()
                {
                    set_state(
                        command,
                        attempt,
                        DispatchState::Joined {
                            handle,
                            session_id: session.id.clone(),
                        },
                    )?;
                } else if attempt.state
                    != (DispatchState::Running {
                        handle: handle.clone(),
                    })
                {
                    set_state(command, attempt, DispatchState::Running { handle })?;
                }
            }
            Ok(ExecutionResult::Attached { handle, session_id }) => {
                let session_id = SessionId(Arc::from(session_id));
                let session = command.exec_report(GetSessionById {
                    id: session_id.clone(),
                })?;
                if session
                    .as_ref()
                    .and_then(|session| session.agent_id.as_deref())
                    == Some(attempt.launch_id.as_str())
                {
                    set_state(
                        command,
                        attempt,
                        DispatchState::Joined { handle, session_id },
                    )?;
                }
            }
            Ok(ExecutionResult::Rejected { reason }) if starting => {
                set_state(command, attempt, DispatchState::Rejected { reason })?;
            }
            Ok(ExecutionResult::Failed { reason }) if !starting => {
                set_state(command, attempt, DispatchState::Rejected { reason })?;
            }
            // Lost start responses and an inconclusive inspection leave the
            // reservation intact. A later pass inspects the same launch ID.
            _ => {}
        }
    }
    Ok(())
}

fn set_state(
    command: &CommandContext,
    attempt: &DispatchAttempt,
    state: DispatchState,
) -> Result<(), CommandError> {
    let mut updated = attempt.clone();
    updated.state = state;
    command.emit_set(&updated)
}
