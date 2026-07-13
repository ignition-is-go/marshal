//! Server-handled `SendMessage` command.
//!
//! Persistence is primary; live push is a best-effort overlay. The pull
//! model decouples acceptance from delivery — the recipient reads on its
//! next turn via the hook, online or not. The handler:
//!   1. resolves the sender — `asSession` for connectionless HTTP-MCP
//!      callers, or the WS `client_id` for shim callers (`resolve_caller`),
//!   2. validates the recipient `Session` exists on the roster (it need
//!      NOT be online),
//!   3. persists the `Message` SET unconditionally,
//!   4. if the recipient has a live WS client, best-effort dispatches a
//!      `NotifyChannel` to surface it immediately (the Track-2 live path);
//!      otherwise the recipient pulls it next turn,
//!   5. returns the result, with `deliveredLive` reflecting whether the
//!      best-effort push landed.
//!
//! Only an unresolvable sender or unknown recipient returns a
//! `CommandError`; an offline recipient is success.

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
    message_read::{MessageRead, MessageReadId},
    notify::NotifyChannel,
    session::{GetAllSessions, Session, SessionId, resolve_caller},
};

#[myko_command(SendMessageResult)]
pub struct SendMessage {
    pub to_session_id: SessionId,
    pub body: String,

    /// Self-identified sender, for callers with no connection identity —
    /// an agent calling this tool over stock myko's HTTP-MCP transport
    /// (no WS `client_id`). The id is the agent's own `session_id`, which
    /// it learned from its SessionStart hook output. WS shim callers omit
    /// this; the server derives the sender from the live connection.
    #[serde(default, rename = "asSession", skip_serializing_if = "Option::is_none")]
    pub as_session: Option<SessionId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub struct SendMessageResult {
    pub message_id: MessageId,
    /// Recipient's session id, echoed back so callers can confirm what
    /// was actually addressed (the daemon's caller resolution can
    /// disambiguate by host+cwd in edge cases the client couldn't see).
    pub to_session_id: SessionId,
    pub sent_at: i64,
    /// `true` only if the recipient had a live WS client AND its session can
    /// actually render channel pushes (claude launched with
    /// `--dangerously-load-development-channels`). `false` if the recipient is
    /// offline OR channels-off — in both cases the message was NOT shown live
    /// and is picked up via the next hook (inbox). Both are success — the
    /// message is always persisted. (channels-off was previously mis-reported
    /// as `true`, so a sender thought a dropped push had landed.)
    #[serde(default)]
    pub delivered_live: bool,
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

        let sender = resolve_caller(&ctx, &sessions, self.as_session.as_ref())?;

        // Resolve the recipient token server-side so every harness (Claude via
        // the shim, the opencode plugin, raw HTTP-MCP) shares ONE resolution
        // policy — resolution used to live only in the shim, so plugin/HTTP
        // callers could address by exact id alone. Accepts id / nickname /
        // operator (human-via-agent) / id-prefix; see `resolve_recipient`.
        let recipient = resolve_recipient(&ctx, &sessions, self.to_session_id.0.as_ref())?;

        let now = Utc::now().timestamp_millis();
        let msg = Message {
            id: MessageId(Arc::from(Uuid::new_v4().to_string())),
            from_session_id: sender.id.clone(),
            to_session_id: Some(recipient.id.clone()),
            to_room_id: None,
            body: self.body.clone(),
            sent_at: now,
        };

        // Persist unconditionally: in the pull model the recipient reads
        // this on its next turn via the hook, whether or not it has a live
        // connection right now. Delivery is decoupled from acceptance.
        ctx.emit_set(&msg)?;

        // Best-effort live push: only when the recipient has a live WS client
        // AND its session can actually render channel pushes. A flag-off
        // recipient (channels_enabled == Some(false)) has a live client but
        // Claude silently drops `notifications/claude/channel`, so a push is a
        // no-op the daemon would otherwise mis-report as delivered_live=true.
        // Treat it as offline: skip the push, report delivered_live=false, and
        // let it pull from the inbox next turn. `None` (legacy shim that
        // doesn't report channel state) keeps prior best-effort behavior.
        let recipient_can_render = recipient.channels_enabled != Some(false);
        // Surface the sender by its memorable nickname so the recipient sees who
        // it's from at a glance. Read the daemon-assigned, frozen handle
        // (`SessionNickname`); fall back to the computed candidate for the brief
        // window before the assignment saga has run. The full id stays in `meta`
        // for exact reply routing; `from_nickname` lets an agent reply by name.
        let from_nickname = crate::nickname_for(&ctx, sender.id.0.as_ref())?;
        let delivered_live = match recipient.client_id.as_ref() {
            Some(cid) if recipient_can_render => push_to_client(
                cid.0.as_ref(),
                // Concise origin ping only — no leading "marshal:" (Claude
                // already prefixes the channel name, else it reads "marshal:
                // marshal:") and no body (it would just be a truncated banner;
                // the full body is in `meta.body`, the persisted Message, and
                // the inbox).
                format!("new message from {from_nickname}"),
                serde_json::json!({
                    "source": "marshal",
                    "kind": "new_message",
                    "from_session": sender.id.0.as_ref(),
                    "from_nickname": from_nickname,
                    "to_session": recipient.id.0.as_ref(),
                    "body": self.body,
                    "sent_at": now,
                }),
            ),
            _ => false,
        };

        // Dedup the pull inbox against a landed live push. A channels-ON
        // recipient already rendered this message via the NotifyChannel, so
        // mark it read for them — the per-turn `<marshal_inbox>` reads only
        // UNREAD direct messages, so it won't re-surface it. Without this the
        // recipient sees every direct message TWICE (once live, once in the
        // inbox). Best-effort: a failed write just falls back to that harmless
        // double-surface.
        if delivered_live {
            let read_id = MessageRead::make_id(msg.id.0.as_ref(), recipient.id.0.as_ref());
            if let Err(e) = ctx.emit_set(&MessageRead {
                id: MessageReadId(Arc::from(read_id.as_str())),
                message_id: msg.id.clone(),
                session_id: recipient.id.clone(),
                read_at: now,
            }) {
                log::warn!("[send_message] live-push inbox dedup write failed: {e:?}");
            }
        }

        Ok(SendMessageResult {
            message_id: msg.id,
            to_session_id: recipient.id.clone(),
            sent_at: now,
            delivered_live,
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

/// Resolve a caller-supplied recipient token against the live roster. Runs
/// server-side (inside the command) rather than in each client, so the shim,
/// the opencode plugin, and raw HTTP-MCP callers all share one policy.
///
/// Priority order:
///   1. exact session id;
///   2. a unique nickname (`swift-falcon`, what the statusline shows);
///   3. an OPERATOR identity — the human, addressed by their resolved operator
///      string (an email like `max@lucid.rocks`, optionally `op:`/`human:`
///      prefixed). This is human-via-agent routing: it delivers to that
///      operator's most-recently-active session, preferring one with a live,
///      render-capable client, so the sender addresses the *person* and the
///      daemon picks which of their agents currently has the floor. If none of
///      the operator's sessions is live the freshest still takes it and reads
///      from its inbox next turn — delivery is decoupled from liveness;
///   4. a unique session-id prefix.
///
/// Ambiguous or unknown tokens error with the candidates listed, so a caller
/// can never silently mis-route.
#[cfg(not(target_arch = "wasm32"))]
fn resolve_recipient(
    ctx: &CommandContext,
    sessions: &[Arc<Session>],
    token: &str,
) -> Result<Arc<Session>, CommandError> {
    // 1. Exact session id.
    if let Some(s) = sessions.iter().find(|s| s.id.0.as_ref() == token) {
        return Ok(s.clone());
    }
    // 2. Unique nickname.
    let mut by_nick: Vec<Arc<Session>> = Vec::new();
    for s in sessions {
        if crate::nickname_for(ctx, s.id.0.as_ref())? == token {
            by_nick.push(s.clone());
        }
    }
    if by_nick.len() == 1 {
        return Ok(by_nick.remove(0));
    }
    if by_nick.len() > 1 {
        return Err(err(ctx, &ambiguous(ctx, "nickname", token, &by_nick)));
    }
    // 3. Operator identity — human-via-agent routing.
    let op = token
        .strip_prefix("op:")
        .or_else(|| token.strip_prefix("human:"))
        .unwrap_or(token);
    if let Some(best) = pick_operator_session(sessions, op) {
        return Ok(best);
    }
    // 4. Unique session-id prefix.
    let by_prefix: Vec<Arc<Session>> = sessions
        .iter()
        .filter(|s| s.id.0.as_ref().starts_with(token))
        .cloned()
        .collect();
    match by_prefix.len() {
        1 => Ok(by_prefix.into_iter().next().unwrap()),
        0 => Err(err(
            ctx,
            &format!(
                "no session matches '{token}' (by id, nickname, operator, or id-prefix) — check marshal://roster"
            ),
        )),
        _ => Err(err(ctx, &ambiguous(ctx, "id-prefix", token, &by_prefix))),
    }
}

/// Best-effort recipient resolution for `@mentions` in a broadcast body:
/// exact id, unique nickname, or operator identity (human-via-agent,
/// most-active). Returns `None` for unknown / ambiguous tokens — a mention is
/// an explicit handle, so we never guess or error on it; an unresolvable
/// `@token` is just prose. Shared with `BroadcastMessage`'s @mention escape
/// hatch. Mirrors tiers 1–3 of `resolve_recipient` minus the prefix tier and
/// the hard errors.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn resolve_mention(
    ctx: &CommandContext,
    sessions: &[Arc<Session>],
    token: &str,
) -> Result<Option<Arc<Session>>, CommandError> {
    // 1. Exact session id.
    if let Some(s) = sessions.iter().find(|s| s.id.0.as_ref() == token) {
        return Ok(Some(s.clone()));
    }
    // 2. Unique nickname (ambiguous → treat as prose, not a hard error).
    let mut by_nick: Vec<Arc<Session>> = Vec::new();
    for s in sessions {
        if crate::nickname_for(ctx, s.id.0.as_ref())? == token {
            by_nick.push(s.clone());
        }
    }
    if by_nick.len() == 1 {
        return Ok(Some(by_nick.remove(0)));
    }
    if by_nick.len() > 1 {
        return Ok(None);
    }
    // 3. Operator identity — human-via-agent.
    let op = token
        .strip_prefix("op:")
        .or_else(|| token.strip_prefix("human:"))
        .unwrap_or(token);
    Ok(pick_operator_session(sessions, op))
}

/// Choose which of an operator's sessions receives a human-via-agent message:
/// the most-recently-active one, with live + render-capable sessions ranked
/// above idle/disconnected ones (the human's current focus). `None` when the
/// operator has no session on the roster. Pure over the roster so the routing
/// policy is unit-testable without a live command context.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn pick_operator_session(sessions: &[Arc<Session>], op: &str) -> Option<Arc<Session>> {
    sessions
        .iter()
        .filter(|s| {
            s.operator
                .as_deref()
                .is_some_and(|o| o.eq_ignore_ascii_case(op))
        })
        .cloned()
        .max_by_key(|s| {
            let live = s.client_id.is_some() && s.channels_enabled != Some(false);
            (live as i64, s.last_activity_at.unwrap_or(s.connected_at))
        })
}

#[cfg(not(target_arch = "wasm32"))]
fn ambiguous(ctx: &CommandContext, kind: &str, token: &str, matches: &[Arc<Session>]) -> String {
    let list = matches
        .iter()
        .map(|s| {
            let nick = crate::nickname_for(ctx, s.id.0.as_ref()).unwrap_or_default();
            format!("{nick} ({})", s.id.0.as_ref())
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "'{token}' is an ambiguous {kind} matching {} sessions [{list}] — address by full session_id",
        matches.len()
    )
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn push_to_client(client_id: &str, content: String, meta: serde_json::Value) -> bool {
    use myko::{command::CommandRequest, server::try_client_registry};
    let Some(registry) = try_client_registry() else {
        log::warn!("[send_message] client_registry not initialized; cannot deliver");
        return false;
    };
    let cmd = NotifyChannel { content, meta };
    let request = CommandRequest::new(cmd);
    registry.send_command_request_to(client_id, &request)
}
