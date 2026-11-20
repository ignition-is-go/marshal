//! Repository work tracked by the Marshal server.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

use myko::command::{CommandContext, CommandError, CommandHandler};
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};
use myko::{myko_command, myko_item, myko_subtype};

#[myko_subtype(derive(Default, Eq, Copy, PartialOrd, Ord))]
pub enum TaskPriority {
    P0,
    P1,
    #[default]
    P2,
    P3,
}

#[myko_subtype(derive(Default, Eq))]
pub enum TaskStatus {
    #[default]
    Open,
    Closed {
        /// Optional evidence for the closure. Status is server-wide.
        commit: Option<String>,
    },
}

#[myko_item]
pub struct Task {
    pub project: String,
    #[myko_setter]
    pub title: String,
    #[myko_setter]
    pub body: String,
    #[myko_setter]
    pub priority: TaskPriority,
    #[myko_setter]
    pub labels: Vec<String>,
    #[myko_setter]
    pub status: TaskStatus,
    /// Creation time in Unix milliseconds.
    pub created: i64,
}

#[myko_command(TaskId)]
pub struct CreateTask {
    pub project: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub priority: TaskPriority,
    #[serde(default)]
    pub labels: Vec<String>,
}

impl CommandHandler for CreateTask {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("CreateTask executes on the server")
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        let project = self.project.trim();
        let title = self.title.trim();
        if project.is_empty() || title.is_empty() {
            return Err(CommandError {
                tx: ctx.tx().to_string(),
                command_id: ctx.command_id.to_string(),
                message: "A task requires a project and title".into(),
            });
        }

        let id = TaskId(Arc::from(uuid::Uuid::now_v7().to_string()));
        ctx.emit_set(&Task {
            id: id.clone(),
            project: project.into(),
            title: title.into(),
            body: self.body,
            priority: self.priority,
            labels: self.labels,
            status: TaskStatus::Open,
            created: chrono::Utc::now().timestamp_millis(),
        })?;
        Ok(id)
    }
}
