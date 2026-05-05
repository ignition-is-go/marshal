//! Server-side reactive logic.
//!
//! Three sagas run inside the daemon:
//! - `MessageNotifySaga` — when a peer SETs a `Message`, push a
//!   `NotifyChannel` at the recipient's WebSocket client so the affected
//!   Claude session sees a `<channel>` block. (`NotifyChannel` is reserved
//!   for inter-agent message delivery; role state is purely declarative
//!   on the `Session` entity and surfaced to Claude by the shim watching
//!   its own session via `GetAllSessions`.)
//! - `DefaultRoleSaga` — when a session is created (its first SET event
//!   from this daemon's perspective), dispatch `RebalanceRoles` to
//!   re-solve the global role assignment so every session — including
//!   the new arrival — ends up with the correct role.
//! - `RebalanceOnSessionDelSaga` — fires on **any** `Session` DEL (not
//!   just task_distributor exits) and dispatches the global
//!   `RebalanceRoles` command. The command re-solves the steady-state
//!   role assignment for every remaining session: most-senior globally is
//!   the communicator; most-senior per cwd is that folder's
//!   task_distributor unless the communicator already lives there;
//!   everyone else is a worker. Emits a Session SET only for sessions
//!   whose role actually changes; the resulting Session entity is then
//!   observed by each shim's local role watcher and surfaced to Claude.

use std::sync::Arc;

use entities::{Message, NotifyChannel, Session, SessionId};
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

// ─── Trigger global rebalance whenever a session arrives without a role ─────

#[myko_saga]
pub struct DefaultRoleSaga;

impl SagaHandler for DefaultRoleSaga {
    type EventItem = Session;
    type Command = crate::rebalance::RebalanceRoles;
    const EVENT_TYPE: MEventType = MEventType::SET;

    fn handle(
        session: Session,
        _event: MEvent,
        _ctx: Arc<SagaContext>,
    ) -> Option<Self::Command> {
        // Fire whenever the post-SET role is None. That covers:
        //   - a brand-new session arriving for the first time,
        //   - a shim reconnect that re-published its `Session` with role
        //     blanked back out (the shim is intentionally not the source
        //     of truth on roles, so its full-Session SETs always carry
        //     `role: None`),
        //   - a daemon restart whose persister loaded sessions but lost
        //     in-memory saga state.
        //
        // RebalanceRoles is idempotent: it only emits Session SETs for
        // sessions whose role *actually* changes, so the SET emitted by
        // a successful rebalance lands here with `role: Some(_)` and the
        // loop terminates after exactly one cycle. Status updates flow
        // through `SetSessionCurrentTask` (a partial-update setter) which
        // preserves the role field, so they don't kick the rebalance.
        if session.role.is_some() {
            return None;
        }
        log::info!(
            "[default-role] session {} in {} has no role; triggering rebalance",
            session.id.0,
            session.cwd,
        );
        Some(crate::rebalance::RebalanceRoles {})
    }
}

// ─── Global rebalance on any session DEL ────────────────────────────────────

#[myko_saga]
pub struct RebalanceOnSessionDelSaga;

impl SagaHandler for RebalanceOnSessionDelSaga {
    type EventItem = Session;
    type Command = crate::rebalance::RebalanceRoles;
    const EVENT_TYPE: MEventType = MEventType::DEL;

    fn handle(
        deleted: Session,
        _event: MEvent,
        _ctx: Arc<SagaContext>,
    ) -> Option<Self::Command> {
        log::info!(
            "[rebalance-on-del] session {} (role {:?}, cwd {}) left; triggering global rebalance",
            deleted.id.0,
            deleted.role,
            deleted.cwd,
        );
        Some(crate::rebalance::RebalanceRoles {})
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn dispatch_notify_channel(client_id: &str, content: String, meta: serde_json::Value) {
    let cmd = NotifyChannel { content, meta };
    let registry = client_registry();
    let request = CommandRequest::new(cmd);
    let dispatched = registry.send_command_request_to(client_id, &request);
    if !dispatched {
        log::warn!(
            "[notify_channel] client {client_id} not in client_registry; dropping notification"
        );
    } else {
        log::debug!("[notify_channel] dispatched to client {client_id}");
    }
}

#[allow(dead_code)]
fn _session_id_marker(s: SessionId) -> SessionId {
    s
}
