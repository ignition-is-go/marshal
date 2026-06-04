//! `ReadMessages` — server-side history fetch with composable filters.
//!
//! After-the-fact only: backlog, history, recap, catch-up after
//! disconnect, bulk-ack flows. Live notifications still flow through
//! `notifications/claude/channel` as messages arrive; clients don't
//! poll this to listen.
//!
//! Filters AND together. Server applies visibility checks first so
//! crafted filters can't leak unauthorised content. `mark_read` writes
//! `MessageRead` rows for the fetched set in the same transaction;
//! idempotent if the row already exists.

use std::sync::Arc;

use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
};
use serde::{Deserialize, Serialize};

use crate::{
    message::{GetAllMessages, Message, MessageId},
    message_read::{GetAllMessageReads, MessageRead},
    room::RoomId,
    room_member::{GetAllRoomMembers, RoomMember},
    session::{GetAllSessions, Session, SessionId},
};

#[myko_command(ReadMessagesResult)]
pub struct ReadMessages {
    /// Filter to messages addressed to this room. Polymorphic id —
    /// `everyone`, `host:*`, `op:*`, `project:*`, or any ad-hoc room
    /// id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<RoomId>,

    /// Filter to messages from this sender (`from_session_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<SessionId>,

    /// Filter to messages addressed to this session_id (direct only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_session: Option<SessionId>,

    /// Restrict to messages addressed to me (direct OR via room
    /// membership). Combine with `unread` for the classic inbox view.
    #[serde(default)]
    pub inbox: bool,

    /// Restrict to messages I sent.
    #[serde(default)]
    pub sent: bool,

    /// Restrict to messages I haven't acked yet (no `MessageRead` row
    /// for me).
    #[serde(default)]
    pub unread: bool,

    /// `sent_at >= since` (millis). Useful for incremental catch-up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,

    /// Cap on returned messages (default 50, max 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadMessagesResult {
    pub messages: Vec<MessageView>,
    /// Number of messages that matched the filters before `limit`
    /// truncation — useful for "showing N of M" UX.
    pub total_matched: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageView {
    pub message_id: MessageId,
    pub from_session_id: SessionId,
    pub from_nick: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_room_id: Option<RoomId>,
    pub to_nick: String,
    pub body: String,
    pub sent_at: i64,
    pub read_by_me: bool,
}

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;

impl CommandHandler for ReadMessages {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("ReadMessages::execute is server-only");
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;
        let me = crate::caller::caller_session(&ctx, &sessions, "read_messages")?.clone();

        let messages: Vec<Arc<Message>> = ctx.exec_query(GetAllMessages {})?;
        let memberships: Vec<Arc<RoomMember>> = ctx.exec_query(GetAllRoomMembers {})?;
        let reads: Vec<Arc<MessageRead>> = ctx.exec_query(GetAllMessageReads {})?;

        // Fast lookups for membership + read state.
        let my_rooms: std::collections::HashSet<RoomId> = memberships
            .iter()
            .filter(|m| m.session_id == me.id)
            .map(|m| m.room_id.clone())
            .collect();
        let my_join_times: std::collections::HashMap<RoomId, i64> = memberships
            .iter()
            .filter(|m| m.session_id == me.id)
            .map(|m| (m.room_id.clone(), m.joined_at))
            .collect();
        let my_reads: std::collections::HashSet<MessageId> = reads
            .iter()
            .filter(|r| r.session_id == me.id)
            .map(|r| r.message_id.clone())
            .collect();

        let limit = self
            .limit
            .map(|n| n.min(MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);

        // Visibility + filter pipeline.
        let visible: Vec<&Arc<Message>> = messages
            .iter()
            .filter(|m| {
                // Visibility: I sent it OR I'm direct recipient OR
                // I'm a current member of the room and joined before
                // it was sent.
                let is_sender = m.from_session_id == me.id;
                let is_direct_recipient = m.to_session_id.as_ref() == Some(&me.id);
                let is_room_member = m
                    .to_room_id
                    .as_ref()
                    .map(|r| my_rooms.contains(r) && joined_before(my_join_times.get(r), m.sent_at))
                    .unwrap_or(false);
                is_sender || is_direct_recipient || is_room_member
            })
            // Apply user filters (AND).
            .filter(|m| match &self.room {
                Some(r) => m.to_room_id.as_ref() == Some(r),
                None => true,
            })
            .filter(|m| match &self.from {
                Some(s) => &m.from_session_id == s,
                None => true,
            })
            .filter(|m| match &self.to_session {
                Some(s) => m.to_session_id.as_ref() == Some(s),
                None => true,
            })
            .filter(|m| !self.inbox || is_addressed_to_me(m, &me.id, &my_rooms))
            .filter(|m| !self.sent || m.from_session_id == me.id)
            .filter(|m| !self.unread || !my_reads.contains(&m.id))
            .filter(|m| match self.since {
                Some(since) => m.sent_at >= since,
                None => true,
            })
            .collect();

        let total_matched = visible.len() as u32;

        // Newest-first; truncate to `limit`.
        let mut sorted: Vec<&Arc<Message>> = visible.into_iter().collect();
        sorted.sort_by(|a, b| b.sent_at.cmp(&a.sent_at));
        sorted.truncate(limit as usize);

        let views: Vec<MessageView> = sorted
            .iter()
            .map(|m| MessageView {
                message_id: m.id.clone(),
                from_session_id: m.from_session_id.clone(),
                from_nick: m.from_nick.clone(),
                to_session_id: m.to_session_id.clone(),
                to_room_id: m.to_room_id.clone(),
                to_nick: m.to_nick.clone(),
                body: m.body.clone(),
                sent_at: m.sent_at,
                read_by_me: my_reads.contains(&m.id),
            })
            .collect();

        Ok(ReadMessagesResult {
            messages: views,
            total_matched,
        })
    }
}

fn joined_before(joined_at: Option<&i64>, sent_at: i64) -> bool {
    joined_at.map(|j| *j <= sent_at).unwrap_or(false)
}

fn is_addressed_to_me(
    message: &Message,
    me: &SessionId,
    my_rooms: &std::collections::HashSet<RoomId>,
) -> bool {
    if message.to_session_id.as_ref() == Some(me) {
        return true;
    }
    if let Some(room) = &message.to_room_id {
        if my_rooms.contains(room) {
            return true;
        }
    }
    false
}

fn err(ctx: &CommandContext, message: &str) -> CommandError {
    CommandError {
        tx: ctx.tx().to_string(),
        command_id: ctx.command_id.to_string(),
        message: message.to_string(),
    }
}
