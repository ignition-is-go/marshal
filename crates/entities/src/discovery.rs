//! Small, read-only discovery results for direct agent coordination.

use myko::command::{CommandContext, CommandError, CommandHandler};
use myko::myko_command;
use serde::{Deserialize, Serialize};

use crate::{SessionId, TaskId};

#[myko_command(CoordinationMatches)]
#[myko_public]
pub struct FindCoordination {
    /// A task project key or repository name from the roster.
    pub project: Option<String>,
    /// Task ID, title words, or label.
    pub query: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub struct CoordinationMatches {
    pub tasks: Vec<TaskMatch>,
    pub idle_agents: Vec<IdleAgent>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub struct TaskMatch {
    pub id: TaskId,
    pub project: String,
    pub title: String,
    pub owner_session_id: Option<SessionId>,
    pub owner_nickname: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub struct IdleAgent {
    pub session_id: SessionId,
    pub nickname: Option<String>,
    pub cwd: String,
    pub host: Option<String>,
    pub last_activity: Option<i64>,
}

#[cfg(target_arch = "wasm32")]
impl CommandHandler for FindCoordination {
    fn execute(self, _: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("FindCoordination executes on the server")
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl CommandHandler for FindCoordination {
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        use crate::{
            GetAllSessionNicknames, GetAllSessions, GetAllTaskClaims, GetAllTasks, TaskStatus,
        };
        use myko::prelude::RequestScoped as _;
        use std::collections::{HashMap, HashSet};

        const RESULT_LIMIT: usize = 12;
        let project = self
            .project
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let query = self
            .query
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        if (project.is_none() && query.is_none())
            || project.is_some_and(|p| p.len() > 512)
            || query.is_some_and(|q| q.len() > 512)
        {
            return Err(CommandError::new(
                ctx.tx(),
                ctx.command_id.to_string(),
                "supply a project or task query of at most 512 bytes",
            ));
        }
        let words: Vec<_> = query
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let now = chrono::Utc::now().timestamp_millis();
        let tasks = ctx.exec_query(GetAllTasks {})?;
        let claims = ctx.exec_query(GetAllTaskClaims {})?;
        let nicknames = ctx.exec_query(GetAllSessionNicknames {})?;
        let names: HashMap<_, _> = nicknames
            .iter()
            .map(|n| (n.id.0.as_ref(), &n.nickname))
            .collect();
        let active: HashMap<_, _> = claims
            .iter()
            .filter(|claim| claim.expires > now)
            .map(|claim| (&claim.task_id, &claim.session_id))
            .collect();
        let mut matching: Vec<_> = tasks
            .iter()
            .filter(|task| {
                if task.status != TaskStatus::Open
                    || project.is_some_and(|project| !project_matches(project, &task.project))
                {
                    return false;
                }
                let title = task.title.to_lowercase();
                words.iter().all(|word| {
                    task.id.0.contains(word.as_str())
                        || title.contains(word)
                        || task
                            .labels
                            .iter()
                            .any(|label| label.to_lowercase().contains(word))
                })
            })
            .collect();
        matching.sort_by_key(|task| {
            (
                !active.contains_key(&task.id),
                task.priority,
                task.created,
                &task.id.0,
            )
        });

        let mut idle_agents = Vec::new();
        if let Some(project) = project {
            let working: HashSet<_> = tasks
                .iter()
                .filter(|task| task.status == TaskStatus::Open)
                .filter_map(|task| active.get(&task.id).copied())
                .collect();
            let sessions = ctx.exec_query(GetAllSessions {})?;
            let mut idle: Vec<_> = sessions
                .iter()
                .filter(|session| {
                    session.activity.as_deref() == Some("idle")
                        && !working.contains(&session.id)
                        && project_matches(
                            project,
                            session
                                .repository
                                .as_deref()
                                .or(session.project.as_deref())
                                .unwrap_or_else(|| basename(&session.cwd)),
                        )
                })
                .collect();
            idle.sort_by_key(|session| {
                (std::cmp::Reverse(session.last_activity_at), &session.id.0)
            });
            idle_agents = idle
                .into_iter()
                .take(RESULT_LIMIT)
                .map(|session| IdleAgent {
                    session_id: session.id.clone(),
                    nickname: names.get(session.id.0.as_ref()).map(|name| (*name).clone()),
                    cwd: session.cwd.chars().take(512).collect(),
                    host: session
                        .host
                        .as_ref()
                        .map(|host| host.name.chars().take(128).collect()),
                    last_activity: session.last_activity_at,
                })
                .collect();
        }

        Ok(CoordinationMatches {
            tasks: matching
                .into_iter()
                .take(RESULT_LIMIT)
                .map(|task| {
                    let owner = active.get(&task.id).copied();
                    TaskMatch {
                        id: task.id.clone(),
                        project: task.project.chars().take(512).collect(),
                        title: task.title.chars().take(256).collect(),
                        owner_session_id: owner.cloned(),
                        owner_nickname: owner
                            .and_then(|id| names.get(id.0.as_ref()))
                            .map(|name| (*name).clone()),
                    }
                })
                .collect(),
            idle_agents,
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn basename(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
}

#[cfg(not(target_arch = "wasm32"))]
fn project_matches(request: &str, stored: &str) -> bool {
    request == stored
        || (!request.contains('/') && request == basename(stored))
        || (!stored.contains('/') && stored == basename(request))
}
