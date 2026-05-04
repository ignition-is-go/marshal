//! Server-side reactive logic.
//!
//! Three sagas run inside the daemon:
//! - `MessageNotifySaga` — when a peer SETs a `Message`, push a
//!   `NotifyChannel` at the recipient's WebSocket client so the affected
//!   Claude session sees a `<channel>` block.
//! - `DefaultRoleSaga` — when a session is created (its first SET event)
//!   with no role set, compute the default role for it (communicator /
//!   task_distributor / worker, by population in the global roster and the
//!   per-cwd subset) and dispatch `SetSessionRole` to apply it.
//! - `RoleChangeNotifySaga` — when a session's role changes, push a
//!   `NotifyChannel` at its client containing the curated instructions for
//!   the new role, so the affected Claude adopts the new behavior.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
};

use entities::{Message, NotifyChannel, Session, SessionId, role_instructions};
use hyphae::Gettable;
use myko::{
    command::CommandRequest,
    core::item::Eventable,
    prelude::myko_saga,
    saga::{SagaContext, SagaHandler},
    utils::downcast_item,
    wire::{MEvent, MEventType},
};
use myko_server::client_registry;

/// Force-link the saga registrations.
pub fn link() {}

// ─── Inbound message → channel push ─────────────────────────────────────────

#[myko_saga]
pub struct MessageNotifySaga;

impl SagaHandler for MessageNotifySaga {
    type EventItem = Message;
    type Command = NotifyChannel;
    const EVENT_TYPE: MEventType = MEventType::SET;

    fn handle(
        message: Message,
        _event: MEvent,
        ctx: Arc<SagaContext>,
    ) -> Option<Self::Command> {
        if message.read_at.is_some() {
            return None;
        }

        let store = ctx.registry.get(Session::ENTITY_NAME_STATIC)?;
        let any_session = store.get(&message.to_session_id.0).get()?;
        let session: Session = downcast_item(&any_session)?;
        let client_id = session.client_id.as_ref()?;

        dispatch_notify_channel(
            client_id.0.as_ref(),
            format!(
                "claude-coord: new message from '{}': {}",
                message.from_nick, message.body
            ),
            serde_json::json!({
                "source": "claude-coord",
                "kind": "new_message",
                "from_nick": message.from_nick,
                "from_session": message.from_session_id.0.as_ref(),
                "to_nick": message.to_nick,
                "body": message.body,
                "sent_at": message.sent_at,
            }),
        );

        None
    }
}

// ─── Default-role assignment on session creation ────────────────────────────

/// Tracks which sessions we've already considered for default-role assignment.
/// We only auto-assign on the *first* SET we see for a session_id; later SETs
/// (e.g. status updates, explicit role clears) don't trigger another
/// assignment.
fn seen_for_default_role() -> &'static Mutex<HashSet<Arc<str>>> {
    static SEEN: OnceLock<Mutex<HashSet<Arc<str>>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

#[myko_saga]
pub struct DefaultRoleSaga;

impl SagaHandler for DefaultRoleSaga {
    type EventItem = Session;
    type Command = entities::SetSessionRole;
    const EVENT_TYPE: MEventType = MEventType::SET;

    fn handle(
        session: Session,
        _event: MEvent,
        ctx: Arc<SagaContext>,
    ) -> Option<Self::Command> {
        // First time we've seen this session id?
        {
            let mut seen = seen_for_default_role().lock().unwrap();
            if !seen.insert(session.id.0.clone()) {
                return None;
            }
        }

        // Only auto-assign if the shim didn't supply one.
        if session.role.is_some() {
            return None;
        }

        let role = compute_default_role(&ctx, &session);
        log::info!(
            "[default-role] session {} (cwd {}) → {role}",
            session.id.0,
            session.cwd
        );

        Some(entities::SetSessionRole {
            id: session.id.clone(),
            role: Some(role.into()),
        })
    }
}

/// Default role per the rules:
/// - first session anywhere → communicator
/// - first session in this cwd → task_distributor
/// - else → worker
fn compute_default_role(ctx: &SagaContext, session: &Session) -> &'static str {
    let Some(store) = ctx.registry.get(Session::ENTITY_NAME_STATIC) else {
        return "communicator";
    };
    let entries = store.entries().get();
    let others: Vec<Session> = entries
        .into_iter()
        .filter_map(|(_id, item)| downcast_item::<Session>(&item))
        .filter(|s| s.id.0 != session.id.0)
        .collect();

    if others.is_empty() {
        "communicator"
    } else if !others.iter().any(|s| s.cwd == session.cwd) {
        "task_distributor"
    } else {
        "worker"
    }
}

// ─── Role-change → channel push ──────────────────────────────────────────────

/// Last-known role per session id. We compare incoming SETs against this so
/// we only fire `NotifyChannel` when the role *actually* changes (not on
/// every status / read_at / unrelated mutation).
fn last_known_roles() -> &'static Mutex<HashMap<Arc<str>, Option<String>>> {
    static ROLES: OnceLock<Mutex<HashMap<Arc<str>, Option<String>>>> = OnceLock::new();
    ROLES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[myko_saga]
pub struct RoleChangeNotifySaga;

impl SagaHandler for RoleChangeNotifySaga {
    type EventItem = Session;
    type Command = NotifyChannel;
    const EVENT_TYPE: MEventType = MEventType::SET;

    fn handle(
        session: Session,
        _event: MEvent,
        _ctx: Arc<SagaContext>,
    ) -> Option<Self::Command> {
        let new_role = session.role.clone();
        {
            let mut state = last_known_roles().lock().unwrap();
            // Flatten Option<Option<_>> so "never seen" and "seen with None"
            // both compare as None — that way the first SET (role=None) does
            // not fire a spurious "cleared" notification.
            let prev_role: Option<String> = state.get(&session.id.0).cloned().flatten();
            if prev_role == new_role {
                return None;
            }
            state.insert(session.id.0.clone(), new_role.clone());
        }

        let client_id = session.client_id.as_ref()?;
        let canonical = new_role
            .as_deref()
            .map(role_instructions::canonicalize)
            .unwrap_or_default();
        let instructions = role_instructions::instructions(&canonical);

        let content = if canonical.is_empty() {
            format!("claude-coord: your role has been cleared.\n\n{instructions}")
        } else {
            format!(
                "claude-coord: your role was set to '{canonical}'.\n\n{instructions}"
            )
        };

        dispatch_notify_channel(
            client_id.0.as_ref(),
            content,
            serde_json::json!({
                "source": "claude-coord",
                "kind": "role_changed",
                "role": canonical,
            }),
        );

        None
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn dispatch_notify_channel(client_id: &str, content: String, meta: serde_json::Value) {
    let cmd = NotifyChannel { content, meta };
    let registry = client_registry();
    let request = CommandRequest::new(cmd);
    let dispatched = registry.send_command_request_to(client_id, &request);
    if !dispatched {
        log::debug!("notify_channel: client {client_id} not connected; dropping");
    }
}

#[allow(dead_code)]
fn _session_id_marker(s: SessionId) -> SessionId {
    s
}
