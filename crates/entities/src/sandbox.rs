//! Imported observations of isolated agent experiments. These are not live sessions.

use std::collections::BTreeMap;
use std::sync::Arc;

use myko::command::{CommandContext, CommandError, CommandHandler};
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};
use myko::{myko_command, myko_item};
use serde::{Deserialize, Serialize};

pub const MAX_SANDBOX_ARTIFACT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub struct SandboxArtifact {
    pub schema_version: u32,
    pub run_id: String,
    pub label: String,
    pub model: String,
    pub scenario: String,
    pub status: SandboxStatus,
    pub tested: bool,
    pub released: bool,
    pub resource_violations: u32,
    pub failed_turns: u32,
    pub messages: u32,
    pub turns: u32,
    pub wake_turns: u32,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub codex_version: String,
    pub binaries: BTreeMap<String, String>,
    pub roles: BTreeMap<String, String>,
    pub instructions: String,
    pub timeline: Vec<SandboxTimelineEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub enum SandboxStatus {
    Starting,
    Running,
    Quiescent,
    BudgetExhausted,
    Deadline,
    ModelTurnFailed,
    Interrupted,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub struct SandboxTimelineEvent {
    pub seconds: f64,
    pub kind: String,
    pub actor: Option<String>,
    pub recipient: Option<String>,
    pub text: String,
}

impl SandboxArtifact {
    pub fn succeeded(&self) -> bool {
        self.status == SandboxStatus::Quiescent
            && self.tested
            && self.released
            && self.resource_violations == 0
            && self.failed_turns == 0
    }

    pub fn actor_name(&self, session_id: &str) -> String {
        self.roles
            .iter()
            .find(|(_, id)| id.as_str() == session_id)
            .map(|(role, _)| role.clone())
            .unwrap_or_else(|| session_id.to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("Unsupported sandbox artifact version".into());
        }
        if self.run_id.is_empty()
            || self.run_id.len() > 128
            || !self
                .run_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err("Invalid sandbox run ID".into());
        }
        if [&self.label, &self.model, &self.scenario]
            .iter()
            .any(|value| value.trim().is_empty() || value.len() > 256)
        {
            return Err("Run label, model, and scenario must contain 1–256 bytes".into());
        }
        if self.cached_input_tokens > self.input_tokens || self.wake_turns > self.turns {
            return Err("Inconsistent sandbox counters".into());
        }
        if self.timeline.len() > 2_000 {
            return Err("Sandbox timeline exceeds 2,000 events".into());
        }
        let mut previous = 0.0;
        for event in &self.timeline {
            if !event.seconds.is_finite() || event.seconds < previous {
                return Err("Sandbox timeline must have finite, ordered timestamps".into());
            }
            previous = event.seconds;
        }
        if serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > MAX_SANDBOX_ARTIFACT_BYTES {
            return Err("Sandbox artifact exceeds 2 MiB".into());
        }
        Ok(())
    }
}

myko::impl_filterable_opaque!(SandboxArtifact);

#[myko_item]
pub struct SandboxRun {
    pub artifact: SandboxArtifact,
    pub imported_at: i64,
}

#[myko_command(SandboxRunId)]
pub struct ImportSandboxRun {
    pub artifact: SandboxArtifact,
}

impl CommandHandler for ImportSandboxRun {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("ImportSandboxRun executes on the daemon")
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        self.artifact.validate().map_err(|message| CommandError {
            tx: ctx.tx().to_string(),
            command_id: ctx.command_id.to_string(),
            message,
        })?;
        let id = SandboxRunId(Arc::from(self.artifact.run_id.as_str()));
        ctx.emit_set(&SandboxRun {
            id: id.clone(),
            artifact: self.artifact,
            imported_at: chrono::Utc::now().timestamp_millis(),
        })?;
        Ok(id)
    }
}
