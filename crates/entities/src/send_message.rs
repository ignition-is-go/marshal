//! Server-handled `SendMessage` command.
//!
//! Live push is the primary purpose; persistence is auxiliary (so peers can
//! browse history later). The handler:
//!   1. resolves the sender from `ctx.client_id()` — the shim never sends
//!      its own id; the server populates it via the client registry,
//!   2. validates the recipient `Session` exists and is bound to a live
//!      WS client (`client_id` populated AND that client is in the live
//!      `client_registry` — a non-`None` value can be stale across a shim
//!      bounce),
//!   3. dispatches a `NotifyChannel` command at the recipient's connection,
//!   4. **only on dispatch success**, persists the `Message` SET so the
//!      transcript reflects what was actually delivered,
//!   5. returns the result for the caller.
//!
//! Failure at any step returns a `CommandError` so the shim's tool surfaces
//! it instead of pretending the message landed.

use std::sync::Arc;

use chrono::Utc;
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    message::{Message, MessageId},
    notify::NotifyChannel,
    session::{GetAllSessions, Session, SessionId},
};

#[myko_command(SendMessageResult)]
pub struct SendMessage {
    pub to_session_id: SessionId,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageResult {
    pub message_id: MessageId,
    pub to_nick: String,
    pub sent_at: i64,
}

impl CommandHandler for SendMessage {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<Self::Result, CommandError> {
        // SendMessage requires the live `client_registry`, which only the
        // server build has. Wasm clients never reach this — the macro skips
        // `register_command_handler!` on wasm — so the body is inert.
        unreachable!("SendMessage::execute is server-only");
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;

        let caller_client_id = ctx
            .client_id()
            .ok_or_else(|| err(&ctx, "send_message must be called over a connected client"))?;
        let sender = sessions
            .iter()
            .find(|s| s.client_id.as_ref() == Some(&caller_client_id))
            .ok_or_else(|| {
                err(
                    &ctx,
                    &format!(
                        "caller (client {}) has no session on the roster — re-SET your Session and retry",
                        caller_client_id.0.as_ref(),
                    ),
                )
            })?;

        let recipient = sessions
            .iter()
            .find(|s| s.id == self.to_session_id)
            .ok_or_else(|| {
                err(
                    &ctx,
                    &format!(
                        "no session with id '{}' on the roster",
                        self.to_session_id.0.as_ref(),
                    ),
                )
            })?;

        let recipient_client_id = recipient.client_id.as_ref().ok_or_else(|| {
            err(
                &ctx,
                &format!(
                    "session '{}' (nickname '{}') is offline — no live client to deliver to",
                    recipient.id.0.as_ref(),
                    recipient.nickname,
                ),
            )
        })?;

        let now = Utc::now().timestamp_millis();
        let msg = Message {
            id: MessageId(Arc::from(Uuid::new_v4().to_string())),
            from_session_id: sender.id.clone(),
            from_nick: sender.nickname.clone(),
            to_session_id: recipient.id.clone(),
            to_nick: recipient.nickname.clone(),
            body: self.body.clone(),
            sent_at: now,
            read_at: None,
        };

        let dispatched = push_to_client(
            recipient_client_id.0.as_ref(),
            format!(
                "marshal: new message from '{}': {}",
                sender.nickname, self.body
            ),
            serde_json::json!({
                "source": "marshal",
                "kind": "new_message",
                "from_nick": sender.nickname,
                "from_session": sender.id.0.as_ref(),
                "to_nick": recipient.nickname,
                "body": self.body,
                "sent_at": now,
            }),
        );
        if !dispatched {
            return Err(err(
                &ctx,
                &format!(
                    "session '{}' (nickname '{}') has a stale client binding ({}); the recipient bounced \
                     and hasn't re-SET its Session yet — retry shortly",
                    recipient.id.0.as_ref(),
                    recipient.nickname,
                    recipient_client_id.0.as_ref(),
                ),
            ));
        }

        // Persist only after the live push succeeds.
        ctx.emit_set(&msg)?;

        Ok(SendMessageResult {
            message_id: msg.id,
            to_nick: recipient.nickname.clone(),
            sent_at: now,
        })
    }
}

fn err(ctx: &CommandContext, message: &str) -> CommandError {
    CommandError {
        tx: ctx.tx().to_string(),
        command_id: ctx.command_id.to_string(),
        message: message.to_string(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn push_to_client(client_id: &str, content: String, meta: serde_json::Value) -> bool {
    use myko::{command::CommandRequest, server::try_client_registry};
    let Some(registry) = try_client_registry() else {
        log::warn!("[send_message] client_registry not initialized; cannot deliver");
        return false;
    };
    let cmd = NotifyChannel { content, meta };
    let request = CommandRequest::new(cmd);
    registry.send_command_request_to(client_id, &request)
}
