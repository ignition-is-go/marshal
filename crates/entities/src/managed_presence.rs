use std::sync::Arc;

use crate::{HostInfo, SessionId};
use myko::prelude::{EventPublishing as _, Querying as _, RequestScoped as _};
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    entities::client::ClientId,
    myko_item,
};
use serde::{Deserialize, Serialize};

pub const MANAGED_PRESENCE_SCAN: std::time::Duration = std::time::Duration::from_secs(30);
pub const MANAGED_PRESENCE_LEASE: std::time::Duration = std::time::Duration::from_secs(90);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub struct ManagedThreadRegistration {
    pub session_id: SessionId,
    pub thread_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub agent_id: Option<String>,
    pub operator: Option<String>,
    pub host: Option<HostInfo>,
}

#[myko_item]
pub struct ManagedPresenceOwner {
    pub client_id: ClientId,
    pub sequence: u64,
    pub threads: Vec<ManagedThreadRegistration>,
    pub observed_tick_ms: u64,
    pub retired: bool,
}

#[myko::myko_command]
#[myko_public]
pub struct ReconcileManagedPresence {
    pub sequence: u64,
    pub threads: Vec<ManagedThreadRegistration>,
    pub retire: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub struct ManagedPresenceSnapshot {
    pub sessions: Vec<SessionId>,
}

#[myko::myko_command(ManagedPresenceSnapshot)]
#[myko_public]
pub struct ReadManagedPresence {}

impl CommandHandler for ReadManagedPresence {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<ManagedPresenceSnapshot, CommandError> {
        unreachable!("managed presence is server-only")
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<ManagedPresenceSnapshot, CommandError> {
        let _guard = session_lifecycle_guard();
        let sessions = current_managed_sessions(&ctx)?;
        Ok(ManagedPresenceSnapshot { sessions })
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn current_managed_sessions(ctx: &CommandContext) -> Result<Vec<SessionId>, CommandError> {
    let clients = ctx
        .exec_query(myko::entities::client::GetAllClients {})?
        .iter()
        .map(|client| client.id.0.clone())
        .collect();
    let now = managed_presence_tick_ms();
    let mut sessions: Vec<SessionId> = ctx
        .exec_query(GetAllManagedPresenceOwners {})?
        .iter()
        .filter(|owner| owner.is_live_for(now, &clients))
        .flat_map(|owner| owner.threads.iter().map(|thread| thread.session_id.clone()))
        .collect();
    sessions.sort_by(|a, b| a.0.cmp(&b.0));
    sessions.dedup();
    Ok(sessions)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn session_lifecycle_guard() -> std::sync::MutexGuard<'static, ()> {
    static LIFECYCLE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LIFECYCLE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn managed_presence_tick_ms() -> u64 {
    static ORIGIN: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);
    ORIGIN.elapsed().as_millis().min(u64::MAX as u128) as u64
}

impl ManagedPresenceOwner {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn is_live_for(&self, now: u64, clients: &std::collections::HashSet<Arc<str>>) -> bool {
        clients.contains(&self.client_id.0) && self.is_live_at(now)
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn is_live_at(&self, now: u64) -> bool {
        !self.retired
            && now.saturating_sub(self.observed_tick_ms) < MANAGED_PRESENCE_LEASE.as_millis() as u64
    }
}

impl CommandHandler for ReconcileManagedPresence {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<(), CommandError> {
        unreachable!("managed presence is server-only")
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<(), CommandError> {
        use crate::GetAllSessions;
        use std::collections::HashSet;
        let error = |message: &str| {
            CommandError::new(
                ctx.req.tx.to_string(),
                ctx.command_id.to_string(),
                message.to_string(),
            )
        };
        let client = ctx
            .client_id()
            .ok_or_else(|| error("Managed presence requires a live connection"))?;
        let _guard = session_lifecycle_guard();
        let clients = ctx.exec_query(myko::entities::client::GetAllClients {})?;
        if !clients.iter().any(|row| row.id.0.as_ref() == client) {
            return Err(error("Managed presence connection is no longer live"));
        }
        if self.sequence == 0
            || self.sequence > i64::MAX as u64
            || self.threads.len() > 128
            || (self.retire && !self.threads.is_empty())
        {
            return Err(error("Invalid managed presence snapshot"));
        }
        let mut ids = HashSet::new();
        for thread in &self.threads {
            crate::session::require_console_owner(&ctx, &thread.session_id)?;
            if thread.session_id.0.is_empty()
                || thread.cwd.is_empty()
                || thread.thread_id.is_empty()
                || !ids.insert(&thread.session_id)
            {
                return Err(error("Invalid or duplicate managed thread identity"));
            }
        }
        let payload = serde_json::to_vec(&self.threads)
            .map_err(|_| error("Invalid managed presence payload"))?;
        if payload.len() > 256 * 1024
            || self.threads.iter().any(|t| {
                t.session_id.0.len() > 256
                    || t.thread_id.len() > 256
                    || t.cwd.len() > 4096
                    || t.title.as_ref().is_some_and(|s| s.len() > 4096)
                    || t.agent_id.as_ref().is_some_and(|s| s.len() > 256)
                    || t.operator.as_ref().is_some_and(|s| s.len() > 256)
                    || t.host.as_ref().is_some_and(|host| {
                        host.name.len() > 256 || host.os.len() > 64 || host.arch.len() > 64
                    })
            })
        {
            return Err(error("Managed presence payload exceeds limits"));
        }
        let owners = ctx.exec_query(GetAllManagedPresenceOwners {})?;
        let prior = owners
            .iter()
            .find(|owner| owner.client_id.0.as_ref() == client);
        let now = managed_presence_tick_ms();
        if let Some(prior) = prior {
            if prior.retired
                && self.retire
                && self.sequence == prior.sequence
                && self.threads == prior.threads
            {
                return Ok(());
            }
            if !prior.is_live_at(now) {
                return Err(error(
                    "Managed presence owner expired or retired; reconnect and rescan",
                ));
            }
            if self.sequence == prior.sequence
                && self.threads == prior.threads
                && self.retire == prior.retired
            {
                return Ok(());
            }
            if self.sequence <= prior.sequence {
                return Err(error("Managed presence sequence must increase"));
            }
        }
        let sessions = ctx.exec_query(GetAllSessions {})?;
        let mut upserts: Vec<Arc<dyn myko::item::AnyItem>> = Vec::new();
        for thread in &self.threads {
            let prior = sessions
                .iter()
                .find(|session| session.id == thread.session_id);
            let mut session = prior.map(|session| (**session).clone()).unwrap_or_else(|| {
                crate::new_hook_session(
                    thread.session_id.clone(),
                    thread.cwd.clone(),
                    chrono::Utc::now().timestamp_millis(),
                )
            });
            session.managed_presence = true;
            session.cwd.clone_from(&thread.cwd);
            if prior.is_none() {
                let nick = crate::nickname_for(&ctx, thread.session_id.0.as_ref())?;
                session.session_name = thread.title.as_ref().map(|title| {
                    title
                        .strip_prefix(&format!("[{nick}] "))
                        .unwrap_or(title)
                        .to_owned()
                });
            }
            if let Some(agent) = thread
                .agent_id
                .as_deref()
                .map(str::trim)
                .filter(|agent| !agent.is_empty())
            {
                session.agent_id = Some(agent.to_owned());
            }
            if session.operator.is_none() {
                session.operator.clone_from(&thread.operator);
            }
            if session.host.is_none() {
                session.host.clone_from(&thread.host);
            }
            if prior.is_none_or(|prior| **prior != session) {
                upserts.push(Arc::new(session));
            }
        }
        upserts.push(Arc::new(ManagedPresenceOwner {
            id: ManagedPresenceOwnerId(Arc::from(client)),
            client_id: ClientId(Arc::from(client)),
            sequence: self.sequence,
            threads: self.threads,
            observed_tick_ms: now,
            retired: self.retire,
        }));
        ctx.emit_replace_any_batch(upserts, Vec::<Arc<dyn myko::item::AnyItem>>::new())
    }
}
