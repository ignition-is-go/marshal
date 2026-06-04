//! Curated MCP surface served direct over HTTP, retiring the shim.
//!
//! Auto-derived `#[myko_command]` / `#[myko_query]` tools (`command_SendMessage`,
//! `query_GetAllSessions`, …) are fine for power users but ugly as a
//! human-facing API. The shim used to translate them into friendlier
//! names (`send_message`, `marshal://roster`) before the daemon offered
//! HTTP-MCP at all. Now that the daemon exposes `/myko/mcp` directly,
//! the same translation lives here as upstream-registered "custom"
//! MCP tools and resources — no separate process required.
//!
//! Each tool handler accepts the friendlier args (e.g. `to_room`/`body`),
//! builds the underlying `#[myko_command]`, and forwards through the
//! same `ctx.execute_command(...)` path the WS shim used to. The MCP
//! caller's `Mcp-Session-Id` rides through `CommandContext.mcp_session_id`
//! so commands resolve the caller via `caller_session()` exactly like
//! the WS path.

use std::sync::Arc;

use myko::{
    command::{CommandContext, CommandHandler},
    request::RequestContext,
};
use myko_server::mcp::{
    CustomMcpRegistry, CustomResource, CustomResourceContext, CustomTool,
};
use serde_json::{Value, json};
use uuid::Uuid;

use marshal_entities::{
    AckMessages, BroadcastMessage, GetAllRoomMembers, GetAllRooms, GetAllSessions, JoinRoom,
    LeaveRoom, MessageId, ReadMessages, Room, RoomId, RoomMember, SendMessage, Session, SessionId,
    SetSessionCurrentTask,
};

/// Register the curated tool + resource set onto an existing
/// `CustomMcpRegistry`. Called once at daemon startup.
pub fn register(registry: &CustomMcpRegistry) {
    for t in tools() {
        registry.register_tool(t);
    }
    for r in resources() {
        registry.register_resource(r);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tools
// ─────────────────────────────────────────────────────────────────────────────

fn tools() -> Vec<CustomTool> {
    vec![
        CustomTool {
            name: "set_status".into(),
            description:
                "Set this session's free-form status text (the `current_task` field on the roster)."
                    .into(),
            input_schema: schema_object(
                json!({
                    "text": {
                        "type": "string",
                        "description": "Free-form status text. Empty string clears.",
                    }
                }),
                &["text"],
            ),
            handler: Arc::new(handle_set_status),
        },
        CustomTool {
            name: "send_message".into(),
            description: "Direct send to a peer's session_id. Look up the id under \
                marshal://roster first; nicknames are display-only and not accepted \
                as recipients. Daemon validates and returns an error if the session \
                is unknown, offline, or has a stale client binding."
                .into(),
            input_schema: schema_object(
                json!({
                    "to": {
                        "type": "string",
                        "description": "Recipient `session_id` (uuid) from marshal://roster. Not a nickname.",
                    },
                    "body": {
                        "type": "string",
                        "description": "Message body.",
                    }
                }),
                &["to", "body"],
            ),
            handler: Arc::new(handle_send_message),
        },
        CustomTool {
            name: "broadcast".into(),
            description: "Fan-out send to every member of a room except yourself. \
                Returns delivered + failed lists. Errors loudly if the room has \
                no other members."
                .into(),
            input_schema: schema_object(
                json!({
                    "to_room": {
                        "type": "string",
                        "description": "Room id from marshal://rooms — `everyone`, `host:*`, `op:*`, `project:*`, or any ad-hoc room id.",
                    },
                    "body": {
                        "type": "string",
                        "description": "Message body.",
                    }
                }),
                &["to_room", "body"],
            ),
            handler: Arc::new(handle_broadcast),
        },
        CustomTool {
            name: "join_room".into(),
            description: "Create or join an ad-hoc room. Reserved prefixes \
                (everyone, host:, op:, project:) are blocked — those auto-rooms \
                are managed by the daemon. Returns whether this call created the \
                room and whether it added a new membership row."
                .into(),
            input_schema: schema_object(
                json!({
                    "name": {
                        "type": "string",
                        "description": "Display name; slugified into the room id (e.g. \"Frontend Redesign\" -> frontend-redesign).",
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional human-readable purpose.",
                    }
                }),
                &["name"],
            ),
            handler: Arc::new(handle_join_room),
        },
        CustomTool {
            name: "leave_room".into(),
            description: "Leave an ad-hoc room. Errors on auto-rooms (their \
                membership is derived from your session's identity)."
                .into(),
            input_schema: schema_object(
                json!({
                    "room": {
                        "type": "string",
                        "description": "Room id (preferred) or original name.",
                    }
                }),
                &["room"],
            ),
            handler: Arc::new(handle_leave_room),
        },
        CustomTool {
            name: "ack_messages".into(),
            description: "Mark message ids as read for this session. Idempotent. \
                Returns counts of newly-acked vs already-acked."
                .into(),
            input_schema: schema_object(
                json!({
                    "message_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Message ids returned by marshal://messages.",
                    }
                }),
                &["message_ids"],
            ),
            handler: Arc::new(handle_ack_messages),
        },
    ]
}

fn handle_set_status(args: Value, ctx: CommandContext) -> Result<Value, String> {
    let text = arg_str(&args, "text", "set_status: missing `text`")?;

    // Resolve the caller's session id (via WS client_id or HTTP
    // mcp_session_id) so we can SET its `current_task` field.
    let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {}).map_err(cmd_err)?;
    let caller = marshal_entities::caller_session(&ctx, &sessions, "set_status").map_err(cmd_err)?;

    let cmd = SetSessionCurrentTask {
        id: caller.id.clone(),
        current_task: if text.is_empty() {
            None
        } else {
            Some(Arc::<str>::from(text.as_str()))
        },
    };
    ctx.execute_command(cmd).map_err(cmd_err)?;
    Ok(json!({ "ok": true }))
}

fn handle_send_message(args: Value, ctx: CommandContext) -> Result<Value, String> {
    let to = arg_str(&args, "to", "send_message: missing `to` (session id)")?;
    let body = arg_str(&args, "body", "send_message: missing `body`")?;
    let cmd = SendMessage {
        to_session_id: SessionId(Arc::<str>::from(to.as_str())),
        body,
    };
    let to_session_id = cmd.to_session_id.0.as_ref().to_string();
    let result = ctx.execute_command(cmd).map_err(cmd_err)?;
    Ok(json!({
        "message_id": result.message_id.0.as_ref(),
        "to_session_id": to_session_id,
        "to_nick": result.to_nick,
        "sent_at": result.sent_at,
    }))
}

fn handle_broadcast(args: Value, ctx: CommandContext) -> Result<Value, String> {
    let to_room = arg_str(&args, "to_room", "broadcast: missing `to_room` (room id)")?;
    let body = arg_str(&args, "body", "broadcast: missing `body`")?;
    let cmd = BroadcastMessage {
        to_room_id: RoomId(Arc::<str>::from(to_room.as_str())),
        body,
    };
    let result = ctx.execute_command(cmd).map_err(cmd_err)?;
    Ok(json!(result))
}

fn handle_join_room(args: Value, ctx: CommandContext) -> Result<Value, String> {
    let name = arg_str(&args, "name", "join_room: missing `name`")?;
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let cmd = JoinRoom { name, description };
    let result = ctx.execute_command(cmd).map_err(cmd_err)?;
    Ok(json!(result))
}

fn handle_leave_room(args: Value, ctx: CommandContext) -> Result<Value, String> {
    let room = arg_str(&args, "room", "leave_room: missing `room` (id or name)")?;
    let cmd = LeaveRoom { room };
    let result = ctx.execute_command(cmd).map_err(cmd_err)?;
    Ok(json!(result))
}

fn handle_ack_messages(args: Value, ctx: CommandContext) -> Result<Value, String> {
    let ids = args
        .get("message_ids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            "ack_messages: missing `message_ids` (array of ids)".to_string()
        })?;
    let message_ids: Vec<MessageId> = ids
        .iter()
        .filter_map(|v| v.as_str())
        .map(|s| MessageId(Arc::<str>::from(s)))
        .collect();
    let cmd = AckMessages { message_ids };
    let result = ctx.execute_command(cmd).map_err(cmd_err)?;
    Ok(json!(result))
}

// ─────────────────────────────────────────────────────────────────────────────
// Resources
// ─────────────────────────────────────────────────────────────────────────────

fn resources() -> Vec<CustomResource> {
    vec![
        CustomResource {
            uri: "marshal://whoami".into(),
            name: "whoami".into(),
            description: "This session's id, nickname, pid, cwd, operator, and host info.".into(),
            mime_type: "application/json".into(),
            handler: Arc::new(handle_whoami),
        },
        CustomResource {
            uri: "marshal://roster".into(),
            name: "roster".into(),
            description: "Every live session with its nickname, cwd, git branch, status, \
                operator, host, and room memberships."
                .into(),
            mime_type: "application/json".into(),
            handler: Arc::new(handle_roster),
        },
        CustomResource {
            uri: "marshal://rooms".into(),
            name: "rooms".into(),
            description: "Every room (auto and ad-hoc) with its members.".into(),
            mime_type: "application/json".into(),
            handler: Arc::new(handle_rooms),
        },
        CustomResource {
            uri: "marshal://messages".into(),
            name: "messages".into(),
            description: "Message history. Query params: room=ID, from=SID, to_session=SID, \
                inbox=true, sent=true, unread=true, since=MILLIS, limit=N. Default returns \
                the 50 most recent messages visible to you (sent, direct-recipient, or via \
                room membership)."
                .into(),
            mime_type: "application/json".into(),
            handler: Arc::new(handle_messages),
        },
    ]
}

fn handle_whoami(uri: &str, res_ctx: CustomResourceContext) -> Result<String, String> {
    let _ = uri;
    let ctx = build_cmd_ctx(&res_ctx, "whoami");
    let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {}).map_err(cmd_err)?;
    let me = marshal_entities::caller_session(&ctx, &sessions, "whoami").map_err(cmd_err)?;
    let value = json!({
        "session_id": me.id.0.as_ref(),
        "nickname": me.nickname,
        "pid": me.pid,
        "cwd": me.cwd,
        "operator": me.operator,
        "host": me.host,
    });
    Ok(pretty(&value))
}

fn handle_roster(uri: &str, res_ctx: CustomResourceContext) -> Result<String, String> {
    let _ = uri;
    let ctx = build_cmd_ctx(&res_ctx, "roster");
    let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {}).map_err(cmd_err)?;
    let members: Vec<Arc<RoomMember>> =
        ctx.exec_query(GetAllRoomMembers {}).map_err(cmd_err)?;
    let me = res_ctx
        .caller_session_id
        .as_deref()
        .unwrap_or("");

    let view: Vec<Value> = sessions
        .iter()
        .map(|s| {
            let rooms: Vec<&str> = members
                .iter()
                .filter(|m| m.session_id == s.id)
                .map(|m| m.room_id.0.as_ref())
                .collect();
            json!({
                "session_id": s.id.0.as_ref(),
                "is_self": s.id.0.as_ref() == me,
                "nickname": s.nickname,
                "pid": s.pid,
                "cwd": s.cwd,
                "git_branch": s.git_branch,
                "current_task": s.current_task,
                "operator": s.operator,
                "host": s.host,
                "connected_at": s.connected_at,
                "rooms": rooms,
            })
        })
        .collect();
    Ok(pretty(&json!({ "sessions": view })))
}

fn handle_rooms(uri: &str, res_ctx: CustomResourceContext) -> Result<String, String> {
    let _ = uri;
    let ctx = build_cmd_ctx(&res_ctx, "rooms");
    let rooms: Vec<Arc<Room>> = ctx.exec_query(GetAllRooms {}).map_err(cmd_err)?;
    let members: Vec<Arc<RoomMember>> =
        ctx.exec_query(GetAllRoomMembers {}).map_err(cmd_err)?;
    let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {}).map_err(cmd_err)?;
    let view: Vec<Value> = rooms
        .iter()
        .map(|r| {
            let room_members: Vec<Value> = members
                .iter()
                .filter(|m| m.room_id == r.id)
                .map(|m| {
                    let nick = sessions
                        .iter()
                        .find(|s| s.id == m.session_id)
                        .map(|s| s.nickname.clone());
                    json!({
                        "session_id": m.session_id.0.as_ref(),
                        "nickname": nick,
                        "joined_at": m.joined_at,
                    })
                })
                .collect();
            json!({
                "room_id": r.id.0.as_ref(),
                "name": r.name,
                "description": r.description,
                "kind": r.kind,
                "created_at": r.created_at,
                "members": room_members,
            })
        })
        .collect();
    Ok(pretty(&json!({ "rooms": view })))
}

fn handle_messages(uri: &str, res_ctx: CustomResourceContext) -> Result<String, String> {
    let query = parse_query(uri);
    let cmd = ReadMessages {
        room: query
            .get("room")
            .cloned()
            .map(|s| RoomId(Arc::from(s.as_str()))),
        from: query
            .get("from")
            .cloned()
            .map(|s| SessionId(Arc::from(s.as_str()))),
        to_session: query
            .get("to_session")
            .cloned()
            .map(|s| SessionId(Arc::from(s.as_str()))),
        inbox: query.get("inbox").map(|v| parse_bool(v)).unwrap_or(false),
        sent: query.get("sent").map(|v| parse_bool(v)).unwrap_or(false),
        unread: query.get("unread").map(|v| parse_bool(v)).unwrap_or(false),
        since: query.get("since").and_then(|s| s.parse::<i64>().ok()),
        limit: query.get("limit").and_then(|s| s.parse::<u32>().ok()),
    };
    let ctx = build_cmd_ctx(&res_ctx, "messages");
    let result = cmd.execute(ctx).map_err(cmd_err)?;
    Ok(pretty(&json!(result)))
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn build_cmd_ctx(res_ctx: &CustomResourceContext, name: &str) -> CommandContext {
    let tx: Arc<str> = Uuid::new_v4().to_string().into();
    let mut req = RequestContext::internal(tx, res_ctx.ctx.host_id, "mcp");
    if let Some(sid) = &res_ctx.caller_session_id {
        req = req.with_mcp_session_id(sid.clone());
    }
    CommandContext::new(Arc::from(name), Arc::new(req), res_ctx.ctx.clone())
}

fn arg_str(args: &Value, key: &str, missing_msg: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| missing_msg.to_string())
}

fn parse_bool(s: &str) -> bool {
    matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "y")
}

fn schema_object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn cmd_err(e: myko::wire::CommandError) -> String {
    e.message
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// Parse a `marshal://path?k=v&k2=v2` URI's query string into a map.
/// Mirrors the shim's hand-rolled `url_decode` so behaviour matches
/// (and we don't pull a URL crate just for this).
fn parse_query(uri: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(rest) = uri.strip_prefix("marshal://") else {
        return out;
    };
    let Some((_, qs)) = rest.split_once('?') else {
        return out;
    };
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((a, b)) => (a, b),
            None => (pair, ""),
        };
        out.insert(url_decode(k).into_owned(), url_decode(v).into_owned());
    }
    out
}

fn url_decode(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('%') && !s.contains('+') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(' '),
            b'%' => {
                let h1 = bytes.next();
                let h2 = bytes.next();
                if let (Some(h1), Some(h2)) = (h1, h2)
                    && let (Some(d1), Some(d2)) =
                        ((h1 as char).to_digit(16), (h2 as char).to_digit(16))
                {
                    out.push(((d1 * 16 + d2) as u8) as char);
                } else {
                    return std::borrow::Cow::Borrowed(s);
                }
            }
            _ => out.push(b as char),
        }
    }
    std::borrow::Cow::Owned(out)
}
