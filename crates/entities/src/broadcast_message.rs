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
use myko::prelude::{EventPublishing as _, RequestScoped as _};
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    message::{CONTEXT_BODY_MAX_CHARS, Message, MessageId, context_preview},
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
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
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
    /// Sessions that an `@mention` in the body resolved to and got a direct
    /// ping (see the @mention escape hatch in `execute`). Empty for an
    /// un-mentioned broadcast. Lets the sender confirm a handle resolved — a
    /// typo'd `@name` simply won't appear here.
    #[serde(default)]
    pub mentioned: Vec<SessionId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub struct DeliveredRecipient {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
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
            id: MessageId(Arc::from(Uuid::now_v7().to_string())),
            from_session_id: sender.id.clone(),
            to_session_id: None,
            to_room_id: Some(room.id.clone()),
            to_operator: None,
            body: self.body.clone(),
            sent_at: now,
        };
        ctx.emit_set(&msg)?;

        // Membership accounting only — no live push. A broadcast is delivered
        // AMBIENTLY (persisted + addressed to the room; seen via the marshal UI
        // or an explicit room read), never blasted into members' turns.
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

            // AMBIENT delivery — a broadcast is NOT real-time-pushed. Unlike a
            // direct message (which auto-injects into the recipient's turn), a
            // room broadcast is persisted + addressed to the room and surfaced
            // ambiently (the marshal UI's room view + an explicit
            // `marshal://messages room=…` read) — never blasted into every
            // member's active context. This is the fix for broadcasts hijacking
            // whole workspaces with unrelated context. `delivered` here means
            // "on the roster + will see it in the room", not "live-pushed".
            // (@mention-driven per-recipient push is the planned opt-in escape
            // hatch; until then, use a direct message when you need eyes-now.)
            let _ = client_id;
            delivered.push(DeliveredRecipient {
                session_id: recipient.id.clone(),
            });
        }

        // ── @mention escape hatch ────────────────────────────────────────
        // A broadcast is ambient, but naming a peer with `@<handle>` in the
        // body is an OPT-IN directed ping. Resolve each @token and deliver it
        // as a real DIRECT message (inbox pull + best-effort live push — the
        // same path as SendMessage) so a mention is never missed, and it works
        // even if the mentioned peer isn't a member of the room. Unresolvable
        // @tokens are just prose and ignored. This is the escape hatch the
        // ambient-broadcast change (#47) left a TODO for.
        let mut mentioned: Vec<SessionId> = Vec::new();
        let mut pinged: std::collections::HashSet<Arc<str>> = std::collections::HashSet::new();
        for token in parse_mentions(&self.body) {
            let Some((target, to_operator)) =
                crate::send_message::resolve_mention(&ctx, &sessions, &token)?
            else {
                continue;
            };
            // Never ping the sender; ping each resolved peer at most once even
            // if it was named twice (e.g. by nickname and by operator).
            if target.id == sender.id || !pinged.insert(target.id.0.clone()) {
                continue;
            }
            let from_nickname = crate::nickname_for(&ctx, sender.id.0.as_ref())?;
            // Persist a direct Message so it lands in the target's inbox and is
            // pulled next turn regardless of live state. The body carries the
            // room context so the recipient knows it came from a broadcast. A
            // human @mention (`@max@lucid.rocks`) carries `to_operator` so it's
            // surfaced to the person, same as a direct human-addressed send.
            let ping = Message {
                id: MessageId(Arc::from(Uuid::now_v7().to_string())),
                from_session_id: sender.id.clone(),
                to_session_id: Some(target.id.clone()),
                to_room_id: None,
                to_operator: to_operator.clone(),
                body: format!("[@mention in {}] {}", room.name, self.body),
                sent_at: now,
            };
            ctx.emit_set(&ping)?;
            let (context_body, body_truncated) =
                context_preview(&ping.body, CONTEXT_BODY_MAX_CHARS);
            // Best-effort live push, honest about render capability (same rule
            // as SendMessage: a flag-off recipient is inbox-only).
            if target.channels_enabled != Some(false)
                && let Some(cid) = target.client_id.as_ref()
            {
                crate::send_message::push_to_client(
                    cid.0.as_ref(),
                    format!("{from_nickname} mentioned you in {}", room.name),
                    serde_json::json!({
                        "source": "marshal",
                        "kind": "mention",
                        "message_id": ping.id.0.as_ref(),
                        "from_session": sender.id.0.as_ref(),
                        "from_nickname": from_nickname,
                        "to_session": target.id.0.as_ref(),
                        "to_operator": to_operator,
                        "room": room.id.0.as_ref(),
                        "body": context_body,
                        "body_truncated": body_truncated,
                        "sent_at": now,
                    }),
                );
            }
            mentioned.push(target.id.clone());
        }

        Ok(BroadcastMessageResult {
            message_id: msg.id,
            to_room_id: room.id.clone(),
            to_room_name: room.name.clone(),
            sent_at: now,
            total: recipient_ids.len() as u32,
            delivered,
            failed,
            mentioned,
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

/// Extract `@mention` handles from a broadcast body. A mention is an `@` at a
/// word boundary (start of string or after whitespace) followed by a handle
/// run: alphanumerics plus `. _ - + : @` — covering nicknames (`swift-falcon`),
/// operator emails (`max@lucid.rocks`), and `op:`/`human:` prefixes. Trailing
/// sentence punctuation is trimmed. The word-boundary rule means a bare email
/// in prose (`ping max@x.com`) is NOT a mention; address the human as
/// `@max@x.com`.
fn parse_mentions(body: &str) -> Vec<String> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let at_boundary = i == 0 || bytes[i - 1].is_ascii_whitespace();
        if bytes[i] == b'@' && at_boundary {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() {
                let c = bytes[j];
                if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'+' | b':' | b'@')
                {
                    j += 1;
                } else {
                    break;
                }
            }
            let tok = body[start..j].trim_end_matches(['.', ',', ';', ':', '!', '?']);
            if !tok.is_empty() {
                out.push(tok.to_string());
            }
            i = j.max(start);
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::parse_mentions;

    #[test]
    fn parses_nickname_email_and_prefixed_mentions() {
        assert_eq!(
            parse_mentions("hey @swift-falcon look"),
            vec!["swift-falcon"]
        );
        assert_eq!(
            parse_mentions("@max@lucid.rocks your call on the redeploy"),
            vec!["max@lucid.rocks"]
        );
        assert_eq!(
            parse_mentions("cc @op:trevor and @human:max"),
            vec!["op:trevor", "human:max"]
        );
        // Trailing sentence punctuation is trimmed off the handle.
        assert_eq!(
            parse_mentions("@swift-falcon, @teal-wolf!"),
            vec!["swift-falcon", "teal-wolf"]
        );
    }

    #[test]
    fn ignores_bare_emails_and_stray_ats() {
        // A bare email in prose is NOT a mention — the '@' isn't at a word
        // boundary. To reach the human you write `@max@x.com`.
        assert!(parse_mentions("ping max@x.com when ready").is_empty());
        assert!(parse_mentions("no mentions here").is_empty());
        assert!(parse_mentions("look @ this").is_empty());
        assert!(parse_mentions("").is_empty());
    }
}
