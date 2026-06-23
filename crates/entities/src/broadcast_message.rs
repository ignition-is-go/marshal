//! Server-handled `BroadcastMessage` command — fan-out delivery to a room.
//!
//! One server call from the sender; the daemon resolves the room's
//! `RoomMember` rows, validates each recipient's live `client_id`, and
//! pushes a `NotifyChannel` per recipient (same wire-level path as
//! `SendMessage`, just iterated). Persists exactly one `Message` row
//! with `to_room_id = Some(room)` and `to_session_id = None`; the
//! per-recipient read state lives on `MessageRead`.
//!
//! Two-tier failure semantics:
//! - **Empty recipient set** (room missing, sender alone) → `CommandError`.
//!   Almost always a user error (forgot to `join_room`, typo in room id).
//! - **Per-recipient stale binding** → fail-soft. The Message persists,
//!   the live push fails for that one recipient, the result's `failed`
//!   list reports it. Downstream state (one stuck peer doesn't block
//!   everyone else) stays correct.

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
    room::{GetAllRooms, Room, RoomId},
    room_member::{GetAllRoomMembers, RoomMember},
    session::{GetAllSessions, Session, SessionId, resolve_caller},
};

#[myko_command(BroadcastMessageResult)]
pub struct BroadcastMessage {
    pub to_room_id: RoomId,
    pub body: String,

    /// Self-identified sender for connectionless paths (HTTP-MCP agents).
    /// WS shim callers omit it. See `resolve_caller`.
    #[serde(default, rename = "asSession", skip_serializing_if = "Option::is_none")]
    pub as_session: Option<SessionId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BroadcastMessageResult {
    pub message_id: MessageId,
    pub to_room_id: RoomId,
    /// Room name at acceptance time. Rooms are user-named, not derived,
    /// so we DO echo this back — it's the routing label the caller
    /// addressed (`join_room`/`broadcast` accept names, not ids only).
    pub to_room_name: String,
    pub sent_at: i64,
    /// Number of recipients the resolution found (excluding sender).
    pub total: u32,
    pub delivered: Vec<DeliveredRecipient>,
    pub failed: Vec<FailedRecipient>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveredRecipient {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedRecipient {
    pub session_id: SessionId,
    pub reason: String,
}

impl CommandHandler for BroadcastMessage {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("BroadcastMessage::execute is server-only");
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        // Resolve sender — WS connection (shim) or self-identified
        // `asSession` (HTTP-MCP agent). Same path as SendMessage.
        let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;
        let sender = resolve_caller(&ctx, &sessions, self.as_session.as_ref())?;

        // Resolve room.
        let rooms: Vec<Arc<Room>> = ctx.exec_query(GetAllRooms {})?;
        let room = rooms
            .iter()
            .find(|r| r.id == self.to_room_id)
            .ok_or_else(|| {
                err(
                    &ctx,
                    &format!(
                        "no room with id '{}' on the roster",
                        self.to_room_id.0.as_ref(),
                    ),
                )
            })?
            .clone();

        // Resolve recipient set: members of the room minus the sender.
        let memberships: Vec<Arc<RoomMember>> = ctx.exec_query(GetAllRoomMembers {})?;
        let recipient_ids: Vec<SessionId> = memberships
            .iter()
            .filter(|m| m.room_id == room.id && m.session_id != sender.id)
            .map(|m| m.session_id.clone())
            .collect();

        if recipient_ids.is_empty() {
            return Err(err(
                &ctx,
                &format!(
                    "room '{}' has no other members — nothing to broadcast to",
                    room.id.0.as_ref(),
                ),
            ));
        }

        // Persist the single Message row up-front so per-recipient
        // failures still leave durable history that the broadcast was
        // attempted.
        let now = Utc::now().timestamp_millis();
        let msg = Message {
            id: MessageId(Arc::from(Uuid::new_v4().to_string())),
            from_session_id: sender.id.clone(),
            to_session_id: None,
            to_room_id: Some(room.id.clone()),
            body: self.body.clone(),
            sent_at: now,
        };
        ctx.emit_set(&msg)?;

        // Per-recipient live push. Stale bindings get logged in `failed`
        // rather than aborting the whole broadcast. Show the sender by its
        // memorable nickname (raw uuid is unreadable); full id stays in `meta`.
        let from_nickname = crate::nickname(sender.id.0.as_ref());
        let mut delivered = Vec::new();
        let mut failed = Vec::new();
        for recipient_id in &recipient_ids {
            let Some(recipient) = sessions.iter().find(|s| &s.id == recipient_id) else {
                failed.push(FailedRecipient {
                    session_id: recipient_id.clone(),
                    reason: "session vanished from roster mid-broadcast".to_string(),
                });
                continue;
            };
            let Some(client_id) = recipient.client_id.as_ref() else {
                // Offline member: not a failure in the pull model — the
                // broadcast Message is already persisted and addressed to
                // the room, so this member picks it up via its next hook.
                // Neither `delivered` (no live push) nor `failed`.
                continue;
            };

            let dispatched = push_to_client(
                client_id.0.as_ref(),
                format!(
                    "marshal: new message from {} to room '{}': {}",
                    from_nickname, room.name, self.body,
                ),
                serde_json::json!({
                    "source": "marshal",
                    "kind": "new_message",
                    "from_session": sender.id.0.as_ref(),
                    "from_nickname": from_nickname,
                    "to_room": room.id.0.as_ref(),
                    "to_room_name": room.name,
                    "body": self.body,
                    "sent_at": now,
                }),
            );
            if dispatched {
                delivered.push(DeliveredRecipient {
                    session_id: recipient.id.clone(),
                });
            } else {
                failed.push(FailedRecipient {
                    session_id: recipient.id.clone(),
                    reason: format!(
                        "stale client binding ({}); recipient bounced and hasn't re-SET",
                        client_id.0.as_ref(),
                    ),
                });
            }
        }

        Ok(BroadcastMessageResult {
            message_id: msg.id,
            to_room_id: room.id.clone(),
            to_room_name: room.name.clone(),
            sent_at: now,
            total: recipient_ids.len() as u32,
            delivered,
            failed,
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
        log::warn!("[broadcast] client_registry not initialized; cannot deliver");
        return false;
    };
    let cmd = NotifyChannel { content, meta };
    let request = CommandRequest::new(cmd);
    registry.send_command_request_to(client_id, &request)
}
