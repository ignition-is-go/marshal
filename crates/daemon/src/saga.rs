//! Server→client push: when a peer SETs a `Message` for a session that's
//! currently connected, dispatch a `NotifyChannel` command at that session's
//! WebSocket client. The shim's `on_command::<NotifyChannel>` handler
//! converts it into an MCP `notifications/claude/channel` event.

use std::sync::Arc;

use entities::{Message, NotifyChannel, Session};
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
        // We only care about the *initial* SET — subsequent SETs (e.g. when a
        // recipient marks the message read) carry a non-None read_at.
        if message.read_at.is_some() {
            return None;
        }

        let store = ctx.registry.get(Session::ENTITY_NAME_STATIC)?;
        let any_session = store.get(&message.to_session_id.0).get()?;
        let session: Arc<Session> = downcast_item(&any_session)?;
        let client_id = session.client_id.as_ref()?;

        let cmd = NotifyChannel {
            content: format!(
                "claude-coord: new message from '{}': {}",
                message.from_nick, message.body
            ),
            meta: serde_json::json!({
                "source": "claude-coord",
                "kind": "new_message",
                "from_nick": message.from_nick,
                "from_session": message.from_session_id.0.as_ref(),
                "to_nick": message.to_nick,
                "body": message.body,
                "sent_at": message.sent_at,
            }),
        };

        let registry = client_registry();
        let request = CommandRequest::new(cmd);
        let dispatched = registry.send_command_request_to(client_id.0.as_ref(), &request);
        if !dispatched {
            log::debug!(
                "MessageNotifySaga: client {} not connected; dropping notification",
                client_id.0
            );
        }

        // The saga has already done its work via client_registry; we don't
        // emit a command into the normal pipeline.
        None
    }
}
