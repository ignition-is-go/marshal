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
    CustomMcpRegistry, CustomResource, CustomResourceContext, CustomResourceHandler, CustomTool,
    CustomToolHandler,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::mcp_observer::LastSeen;
use marshal_entities::{
    AckMessages, BroadcastMessage, GetAllRoomMembers, GetAllRooms, GetAllSessions, JoinRoom,
    LeaveRoom, MessageId, ReadMessages, Room, RoomId, RoomMember, SendMessage, Session, SessionId,
    SetSessionCurrentTask,
};

/// Register the curated tool + resource set onto an existing
/// `CustomMcpRegistry`. Called once at daemon startup.
///
/// `last_seen` is the same liveness map the sweeper consults. Curated
/// calls that act AS a session (register, or any `as_session` write /
/// `as_session` read) bump it for that session id, so hook-registered
/// sessions — which have no connection-bound liveness signal — survive
/// the sweeper as long as their hooks keep firing within the grace
/// window. `deregister` (SessionEnd hook) removes them cleanly; the
/// grace window is the crash fallback.
pub fn register(registry: &CustomMcpRegistry, last_seen: LastSeen) {
    for t in tools(&last_seen) {
        registry.register_tool(t);
    }
    for r in resources(&last_seen) {
        registry.register_resource(r);
    }
}

/// Bump liveness for the session a curated write call is acting as.
fn bump_as_session(last_seen: &LastSeen, args: &Value) {
    if let Some(s) = args
        .get("as_session")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        last_seen.touch(s.to_string());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tools
// ─────────────────────────────────────────────────────────────────────────────

fn tools(last_seen: &LastSeen) -> Vec<CustomTool> {
    // Each write handler is wrapped so that acting AS a session bumps its
    // liveness. `register` bumps the registered id directly (below).
    let wrap = |inner: fn(Value, CommandContext) -> Result<Value, String>| -> CustomToolHandler {
        let ls = last_seen.clone();
        Arc::new(move |args: Value, ctx: CommandContext| {
            bump_as_session(&ls, &args);
            inner(args, ctx)
        })
    };
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
            handler: wrap(handle_set_status),
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
            handler: wrap(handle_send_message),
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
            handler: wrap(handle_broadcast),
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
            handler: wrap(handle_join_room),
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
            handler: wrap(handle_leave_room),
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
            handler: wrap(handle_ack_messages),
        },
        CustomTool {
            name: "register".into(),
            description: "Upsert this session's roster entry, keyed by the supplied \
                session_id (use your Claude Code session_id so peers, the inbox \
                query, and the statusline all agree on one id). Called by the \
                SessionStart hook. Idempotent across resume — preserves prior status."
                .into(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["session_id", "nickname"],
                "properties": {
                    "session_id": { "type": "string", "description": "Your Claude Code session_id (uuid). Becomes the roster id peers address." },
                    "nickname":   { "type": "string", "description": "Display name, e.g. <dir>@<short-id>." },
                    "cwd":        { "type": "string", "description": "Working directory." },
                    "git_branch": { "type": "string", "description": "Git branch in cwd, if any." },
                    "operator":   { "type": "string", "description": "Human operator (USER)." },
                    "project":    { "type": "string", "description": "Project basename — anchors the project:* auto-room." },
                    "pid":        { "type": "integer", "description": "Claude Code parent pid." },
                    "host": {
                        "type": "object",
                        "description": "Host info — anchors the host:* auto-room.",
                        "properties": {
                            "name": { "type": "string" },
                            "os":   { "type": "string" },
                            "arch": { "type": "string" }
                        }
                    }
                }
            }),
            handler: {
                let ls = last_seen.clone();
                Arc::new(move |args: Value, ctx: CommandContext| {
                    if let Some(s) = args.get("session_id").and_then(|v| v.as_str()) {
                        ls.touch(s.to_string());
                    }
                    handle_register(args, ctx)
                })
            },
        },
        CustomTool {
            name: "deregister".into(),
            description: "Remove this session's roster entry. Called by the SessionEnd \
                hook so a cleanly-closed session disappears immediately. Idempotent."
                .into(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["session_id"],
                "properties": {
                    "session_id": { "type": "string", "description": "The session_id you registered with." }
                }
            }),
            handler: Arc::new(handle_deregister),
        },
    ]
}

fn handle_set_status(args: Value, ctx: CommandContext) -> Result<Value, String> {
    let ctx = maybe_act_as(&args, ctx);
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
    let ctx = maybe_act_as(&args, ctx);
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
    let ctx = maybe_act_as(&args, ctx);
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
    let ctx = maybe_act_as(&args, ctx);
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
    let ctx = maybe_act_as(&args, ctx);
    let room = arg_str(&args, "room", "leave_room: missing `room` (id or name)")?;
    let cmd = LeaveRoom { room };
    let result = ctx.execute_command(cmd).map_err(cmd_err)?;
    Ok(json!(result))
}

fn handle_ack_messages(args: Value, ctx: CommandContext) -> Result<Value, String> {
    let ctx = maybe_act_as(&args, ctx);
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

fn resources(last_seen: &LastSeen) -> Vec<CustomResource> {
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
            handler: {
                let ls = last_seen.clone();
                Arc::new(move |uri: &str, res_ctx: CustomResourceContext| {
                    // The inbox read acts as `as_session`; bump its liveness
                    // so a session polled only via its UserPromptSubmit hook
                    // survives the sweeper between turns.
                    if let Some(s) = parse_query(uri).get("as_session") {
                        ls.touch(s.clone());
                    }
                    handle_messages(uri, res_ctx)
                }) as CustomResourceHandler
            },
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
    let ctx = build_cmd_ctx_as(&res_ctx, "messages", query.get("as_session").map(|s| s.as_str()));
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
    let result = cmd.execute(ctx).map_err(cmd_err)?;
    Ok(pretty(&json!(result)))
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn build_cmd_ctx(res_ctx: &CustomResourceContext, name: &str) -> CommandContext {
    build_cmd_ctx_as(res_ctx, name, None)
}

/// Like `build_cmd_ctx`, but lets a resource act AS an explicit session
/// id (from an `as_session` query param) rather than the connection's
/// Mcp-Session-Id. The hook-based clients use this to read "messages to
/// ME" by declaring their cc_session_id, since the daemon-minted
/// connection id has no matching Session entity. Falls back to the
/// connection identity when `as_session` is absent.
fn build_cmd_ctx_as(
    res_ctx: &CustomResourceContext,
    name: &str,
    as_session: Option<&str>,
) -> CommandContext {
    let tx: Arc<str> = Uuid::new_v4().to_string().into();
    let mut req = RequestContext::internal(tx, res_ctx.ctx.host_id, "mcp");
    let sid: Option<Arc<str>> = match as_session {
        Some(s) if !s.is_empty() => Some(Arc::from(s)),
        _ => res_ctx.caller_session_id.clone(),
    };
    if let Some(sid) = sid {
        req = req.with_mcp_session_id(sid);
    }
    CommandContext::new(Arc::from(name), Arc::new(req), res_ctx.ctx.clone())
}

/// Override the acting identity of a curated *write* command when the
/// caller passes an explicit `as_session` argument. This is how the
/// hook-based HTTP clients act AS their `cc_session_id` without a
/// connection-bound identity: `caller_session()` resolves via
/// `mcp_session_id` when `client_id` is absent, so we clone the ctx,
/// clear `client_id`, and set `mcp_session_id` to the declared id.
/// Absent/empty `as_session` leaves the connection identity untouched —
/// the WS shim and legacy HTTP path keep resolving caller from the
/// connection.
fn maybe_act_as(args: &Value, ctx: CommandContext) -> CommandContext {
    match args.get("as_session").and_then(|v| v.as_str()) {
        Some(sid) if !sid.is_empty() => {
            let mut new_ctx = ctx.clone();
            let mut req = (*ctx.req).clone();
            req.client_id = None;
            req.mcp_session_id = Some(Arc::from(sid));
            new_ctx.req = Arc::new(req);
            new_ctx
        }
        _ => ctx,
    }
}

fn host_info_from_json(v: &Value) -> Option<marshal_entities::HostInfo> {
    Some(marshal_entities::HostInfo {
        name: v.get("name")?.as_str()?.to_string(),
        os: v.get("os").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        arch: v.get("arch").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    })
}

/// Upsert this session's roster entry. Called by the SessionStart hook
/// with the agent's `cc_session_id` as `session_id`, so the roster keys
/// on the same id peers address and the statusline shows — no
/// connection-bound identity, no shim-picked uuid. Idempotent across
/// resume: preserves the prior `current_task` + original `connected_at`
/// when the session already exists.
fn handle_register(args: Value, ctx: CommandContext) -> Result<Value, String> {
    let session_id = arg_str(&args, "session_id", "register: missing `session_id`")?;
    let nickname = arg_str(&args, "nickname", "register: missing `nickname`")?;
    let cwd = args.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let git_branch = args
        .get("git_branch")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let operator = args
        .get("operator")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let project = args
        .get("project")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let pid = args.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let host = args.get("host").and_then(host_info_from_json);

    let sid = SessionId(Arc::from(session_id.as_str()));
    let existing: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {}).map_err(cmd_err)?;
    let prior = existing.iter().find(|s| s.id == sid);
    let now = chrono::Utc::now().timestamp_millis();

    let session = Session {
        id: sid,
        client_id: None,
        nickname,
        pid,
        cwd,
        git_branch,
        current_task: prior.and_then(|p| p.current_task.clone()),
        connected_at: prior.map(|p| p.connected_at).unwrap_or(now),
        last_activity_at: Some(now),
        last_tool: None,
        last_tool_at: None,
        operator,
        host,
        project,
    };
    let resumed = prior.is_some();
    ctx.emit_set(&session).map_err(cmd_err)?;
    Ok(json!({ "ok": true, "session_id": session_id, "resumed": resumed }))
}

/// Remove this session's roster entry. Called by the SessionEnd hook so
/// a cleanly-closed session disappears immediately rather than waiting
/// for the staleness sweeper. Idempotent — DEL of a missing id is a
/// no-op.
fn handle_deregister(args: Value, ctx: CommandContext) -> Result<Value, String> {
    let session_id = arg_str(&args, "session_id", "deregister: missing `session_id`")?;
    let stub = Session {
        id: SessionId(Arc::from(session_id.as_str())),
        client_id: None,
        nickname: String::new(),
        pid: 0,
        cwd: String::new(),
        git_branch: None,
        current_task: None,
        connected_at: 0,
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
        operator: None,
        host: None,
        project: None,
    };
    ctx.emit_del(&stub).map_err(cmd_err)?;
    Ok(json!({ "ok": true }))
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

/// Build a write-tool input schema. Every write tool also accepts an
/// optional `as_session` — the cc_session_id the caller is acting as —
/// so hook-based HTTP clients can attribute calls to their registered
/// identity without a connection-bound one (see `maybe_act_as`).
fn schema_object(properties: Value, required: &[&str]) -> Value {
    let mut props = properties;
    if let Some(obj) = props.as_object_mut() {
        obj.entry("as_session").or_insert(json!({
            "type": "string",
            "description": "Act as this session id (your cc_session_id). Omit when connected as a single identity (WS shim).",
        }));
    }
    json!({
        "type": "object",
        "properties": props,
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
