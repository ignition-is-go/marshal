//! `AckMessages` — write tool that marks messages read for the calling
//! session.
//!
//! Split from `ReadMessages` so the read path stays a pure resource
//! (no side effects) and the mutation lives in its own tool. Idempotent:
//! re-acking a message that's already been acked is a no-op.

use std::sync::Arc;

use chrono::Utc;
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
};
use serde::{Deserialize, Serialize};

use crate::{
    message::MessageId,
    message_read::{GetAllMessageReads, MessageRead, MessageReadId},
    session::{GetAllSessions, Session},
};

#[myko_command(AckMessagesResult)]
pub struct AckMessages {
    /// Messages to mark read. Order doesn't matter; missing/unknown
    /// ids are silently skipped (idempotent — re-acking is fine).
    pub message_ids: Vec<MessageId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AckMessagesResult {
    /// Number of `MessageRead` rows newly written. Re-acks of already-read
    /// messages don't count here.
    pub newly_acked: u32,
    /// Number of input ids that were already acked before this call.
    pub already_acked: u32,
}

impl CommandHandler for AckMessages {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("AckMessages::execute is server-only");
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;
        let me = crate::caller::caller_session(&ctx, &sessions, "ack_messages")?.clone();

        let reads: Vec<Arc<MessageRead>> = ctx.exec_query(GetAllMessageReads {})?;
        let already: std::collections::HashSet<MessageId> = reads
            .iter()
            .filter(|r| r.session_id == me.id)
            .map(|r| r.message_id.clone())
            .collect();

        let now = Utc::now().timestamp_millis();
        let mut newly_acked = 0u32;
        let mut already_acked = 0u32;
        for message_id in &self.message_ids {
            if already.contains(message_id) {
                already_acked += 1;
                continue;
            }
            let id_str = MessageRead::make_id(message_id.0.as_ref(), me.id.0.as_ref());
            ctx.emit_set(&MessageRead {
                id: MessageReadId(Arc::from(id_str.as_str())),
                message_id: message_id.clone(),
                session_id: me.id.clone(),
                read_at: now,
            })?;
            newly_acked += 1;
        }

        Ok(AckMessagesResult {
            newly_acked,
            already_acked,
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
