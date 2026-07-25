//! MCP tool + resource dispatch.
//!
//! Read paths are MCP resources (`marshal://whoami`, `marshal://roster`,
//! `marshal://rooms`, `marshal://messages?...`); they're idempotent
//! fetches with no side effects. Write paths are MCP tools
//! (`send_message`, `broadcast`, `join_room`, `leave_room`,
//! `set_status`, `ack_messages`); they mutate.
//!
//! All write tools route through server-side commands so validation +
//! persistence happen in one place. Read resources also call server
//! commands (the `ReadMessages` family is server-side too) but expose
//! the result as a resource read instead of a tool call.

use crate::mcp::{
    Handler, METHOD_NOT_FOUND, Notifier, ResourceContent, ResourceDef, ResourceError,
    ResourceFuture, ToolDef, ToolError, ToolFuture, ToolOutcome,
};
use hyphae::{Cell, CellImmutable, Gettable, Signal, Watchable};
use marshal_entities::{
    AckMessages, AckMessagesResult, BroadcastMessage, BroadcastMessageResult, JoinRoom,
    JoinRoomResult, LeaveRoom, LeaveRoomResult, MessageId, ReadMessages, ReadMessagesResult, Room,
    RoomId, RoomMember, SendMessage, SendMessageResult, Session, SessionId, SetSessionCurrentTask,
};
use myko::client::MykoClient;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct ToolHost {
    pub client: Arc<MykoClient>,
    pub pid: u32,
    pub cwd: String,
    /// True under the Codex harness. Codex gives the shim no connection
    /// identity and disk discovery is unreliable, so the shim owns no Session
    /// and write tools carry an explicit `asSession` (the id the SessionStart
    /// hook injected) instead of resolving the caller from the WS connection.
    pub is_codex: bool,
    /// The shim's local copy of its Session entity. Mutations
    /// (set_status) update this and re-emit a SET event so the
    /// server's view stays in sync. A placeholder under Codex (never
    /// published — the SessionStart hook owns the real Session row).
    pub session: Arc<Mutex<Session>>,
    /// Long-lived watch_query subscriptions held warm so resources
    /// can read a primed cache without racing the server's first
    /// response.
    pub sessions_cell: Cell<Vec<Arc<Session>>, CellImmutable>,
    pub rooms_cell: Cell<Vec<Arc<Room>>, CellImmutable>,
    pub members_cell: Cell<Vec<Arc<RoomMember>>, CellImmutable>,
    /// Daemon-assigned handles, keyed by session id. Read instead of
    /// recomputing so a wordlist change never desyncs the shim from the roster.
    pub nicknames_cell: Cell<Vec<Arc<marshal_entities::SessionNickname>>, CellImmutable>,
}

/// The daemon-assigned handle for `session_id`, read from the SessionNickname
/// cell. Falls back to the computed candidate for the brief window before the
/// daemon has assigned one — they agree in the common case, and the assigned
/// value is authoritative once present.
fn handle_for(host: &ToolHost, session_id: &str) -> String {
    host.nicknames_cell
        .get()
        .iter()
        .find(|n| n.id.0.as_ref() == session_id)
        .map(|n| n.nickname.clone())
        .unwrap_or_else(|| marshal_entities::nickname(session_id))
}

pub struct CoordHandler {
    pub host: Arc<ToolHost>,
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

impl Handler for CoordHandler {
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        _notifier: &'a Notifier,
    ) -> ToolFuture<'a> {
        let host = Arc::clone(&self.host);
        let args = args.clone();
        let name = name.to_string();
        Box::pin(async move {
            match name.as_str() {
                "set_status" => set_status(&host, &args).await,
                "send_message" => send_message(&host, &args).await,
                "broadcast" => broadcast(&host, &args).await,
                "join_room" => join_room(&host, &args).await,
                "leave_room" => leave_room(&host, &args).await,
                "ack_messages" => ack_messages(&host, &args).await,
                other => Err(ToolError {
                    code: METHOD_NOT_FOUND,
                    message: format!("unknown tool: {other}"),
                    data: None,
                }),
            }
        })
    }

    fn read_resource<'a>(&'a self, uri: &'a str) -> ResourceFuture<'a> {
        let host = Arc::clone(&self.host);
        let uri = uri.to_string();
        Box::pin(async move {
            let parsed = ParsedUri::parse(&uri)?;
            match parsed.path.as_str() {
                "whoami" => Ok(read_whoami(&host, &uri, &parsed.query)),
                "roster" => Ok(read_roster(&host, &uri)),
                "rooms" => Ok(read_rooms(&host, &uri)),
                "messages" => read_messages(&host, &uri, &parsed.query).await,
                other => Err(ResourceError {
                    code: METHOD_NOT_FOUND,
                    message: format!("no resource at 'marshal://{other}'"),
                    data: None,
                }),
            }
        })
    }
}

// =============================================================================
// Resource implementations (read-only)
// =============================================================================

fn read_whoami(
    host: &ToolHost,
    uri: &str,
    query: &std::collections::HashMap<String, String>,
) -> ResourceContent {
    // Under Codex the shim holds no connection identity — `host.session` is a
    // placeholder id, so reporting it here would tell the agent the WRONG roster
    // nickname. The authoritative identity is the one the SessionStart hook
    // injected in the <marshal_session> block; the agent names it via ?asSession=,
    // exactly as it does on write tools. With the id we return the live roster row.
    if host.is_codex {
        let as_session = query
            .get("asSession")
            .or_else(|| query.get("as_session"))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        return match as_session {
            Some(sid) => {
                let sessions = host.sessions_cell.get();
                match sessions.iter().find(|s| s.id.0.as_ref() == sid) {
                    Some(s) => json_resource(
                        uri,
                        json!({
                            "session_id": s.id.0.as_ref(),
                            "nickname": handle_for(host, s.id.0.as_ref()),
                            "pid": s.pid,
                            "cwd": s.cwd,
                            "operator": s.operator,
                            "host": s.host,
                            "harness": "codex",
                        }),
                    ),
                    // Given, but not on the roster yet (SessionStart hook lag).
                    None => json_resource(
                        uri,
                        json!({
                            "session_id": sid,
                            "nickname": handle_for(host, sid),
                            "harness": "codex",
                            "note": "This id isn't on the live roster yet (the SessionStart hook may not have registered it); identity is derived from the id.",
                        }),
                    ),
                }
            }
            None => json_resource(
                uri,
                json!({
                    "session_id": null,
                    "nickname": null,
                    "harness": "codex",
                    "note": "Under Codex this MCP server isn't told which session it serves. Your identity is the session_id in your <marshal_session> block — pass it as ?asSession=<id> here (marshal://whoami?asSession=<id>) and as asSession=<id> on every write tool.",
                }),
            ),
        };
    }
    let snapshot = host.session.lock().unwrap().clone();
    json_resource(
        uri,
        json!({
            "session_id": snapshot.id.0.as_ref(),
            "nickname": handle_for(host, snapshot.id.0.as_ref()),
            "pid": host.pid,
            "cwd": host.cwd,
            "operator": snapshot.operator,
            "host": snapshot.host,
        }),
    )
}

fn read_roster(host: &ToolHost, uri: &str) -> ResourceContent {
    let sessions: Vec<Arc<Session>> = host.sessions_cell.get();
    let members: Vec<Arc<RoomMember>> = host.members_cell.get();
    let me = host.session.lock().unwrap().id.0.to_string();
    let now_ms = chrono::Utc::now().timestamp_millis();
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
                "nickname": handle_for(host, s.id.0.as_ref()),
                "is_self": s.id.0.as_ref() == me.as_str(),
                "pid": s.pid,
                "cwd": s.cwd,
                "git_branch": s.git_branch,
                "current_task": s.current_task,
                "operator": s.operator,
                "host": s.host,
                "project": s.project,
                "connected_at": s.connected_at,
                // Liveness — so an AGENT reading the roster (not just the human
                // UI) can tell an actively-working peer from an idle one or a
                // ghost pending sweep. Raw activity timestamps + a derived
                // "how long since last seen" (falls back to connected_at).
                "last_activity_at": s.last_activity_at,
                "last_tool": s.last_tool,
                "last_tool_at": s.last_tool_at,
                "last_seen_ms_ago": now_ms - s.last_activity_at.unwrap_or(s.connected_at),
                "rooms": rooms,
            })
        })
        .collect();
    json_resource(uri, json!({ "sessions": view }))
}

fn read_rooms(host: &ToolHost, uri: &str) -> ResourceContent {
    let rooms: Vec<Arc<Room>> = host.rooms_cell.get();
    let members: Vec<Arc<RoomMember>> = host.members_cell.get();
    let sessions: Vec<Arc<Session>> = host.sessions_cell.get();
    let view: Vec<Value> = rooms
        .iter()
        .map(|r| {
            let room_members: Vec<Value> = members
                .iter()
                .filter(|m| m.room_id == r.id)
                .map(|m| {
                    let member = sessions.iter().find(|s| s.id == m.session_id);
                    json!({
                        "session_id": m.session_id.0.as_ref(),
                        "host": member.and_then(|s| s.host.as_ref().map(|h| h.name.clone())),
                        "cwd": member.map(|s| s.cwd.clone()),
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
    json_resource(uri, json!({ "rooms": view }))
}

async fn read_messages(
    host: &ToolHost,
    uri: &str,
    query: &std::collections::HashMap<String, String>,
) -> Result<ResourceContent, ResourceError> {
    // Under Codex the shim has no connection identity, so the daemon can't resolve
    // whose inbox/sent to read — the agent names itself via ?asSession= (the same id
    // it passes to write tools). Every other harness resolves the caller from the WS
    // connection, so this stays None.
    let as_session = if host.is_codex {
        match query
            .get("asSession")
            .or_else(|| query.get("as_session"))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            Some(sid) => Some(SessionId(Arc::from(sid))),
            None => {
                return Err(ResourceError::invalid_params(
                    "Under Codex, marshal://messages needs your session id: pass \
                     ?asSession=<the id from your <marshal_session> block>, e.g. \
                     marshal://messages?asSession=<id>&inbox=true. (This MCP server \
                     isn't told which Codex session it serves.)",
                ));
            }
        }
    } else {
        None // WS path: caller resolved from the connection
    };
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
        as_session,
    };
    let cell = host
        .client
        .send_command::<ReadMessages, ReadMessagesResult>(&cmd);
    let result = await_command(cell, REQUEST_TIMEOUT)
        .await
        .map_err(ResourceError::invalid_params)?;
    Ok(json_resource(uri, json!(result)))
}

// =============================================================================
// Tool implementations (writes)
// =============================================================================

/// The session a write command acts AS. Under Codex the shim holds no
/// connection identity, so the agent names its own session explicitly via
/// `asSession` (the id the SessionStart hook injected in the `<marshal_session>`
/// block) and the shim forwards it. Every other harness resolves the caller
/// from the WS connection, so this is `None`.
fn caller(host: &ToolHost, args: &Value) -> Result<Option<SessionId>, ToolError> {
    if !host.is_codex {
        return Ok(None);
    }
    let s = args
        .get("asSession")
        .or_else(|| args.get("as_session"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ToolError::invalid_params(
                "missing `asSession`: pass your own marshal session id (shown in the \
                 <marshal_session> block at session start) so peers know who sent this",
            )
        })?;
    Ok(Some(SessionId(Arc::from(s))))
}

async fn set_status(host: &ToolHost, args: &Value) -> Result<ToolOutcome, ToolError> {
    let text = arg_str(args, "text", "set_status: missing `text`")?;
    let new_task = if text.is_empty() {
        None
    } else {
        Some(Arc::<str>::from(text.as_str()))
    };
    // Codex: target the agent's own session (asSession); else our own row.
    let id = caller(host, args)?.unwrap_or_else(|| host.session.lock().unwrap().id.clone());
    let _ = host
        .client
        .send_command::<SetSessionCurrentTask, ()>(&SetSessionCurrentTask {
            id,
            current_task: new_task,
        });
    if !host.is_codex {
        let mut sess = host.session.lock().unwrap();
        sess.current_task = if text.is_empty() { None } else { Some(text) };
    }
    Ok(ToolOutcome::Json(json!({ "ok": true })))
}

async fn send_message(host: &ToolHost, args: &Value) -> Result<ToolOutcome, ToolError> {
    let to = arg_str(
        args,
        "to",
        "send_message: missing `to` (session id or nickname)",
    )?;
    let body = arg_str(args, "body", "send_message: missing `body`")?;
    // Recipient resolution is authoritative in the daemon's SendMessage command
    // now (so every harness shares one policy AND operator / human-via-agent
    // addressing works from the shim too) — pass the raw token straight through.
    let to_session_id = SessionId(std::sync::Arc::from(to.as_str()));
    let cmd = SendMessage {
        to_session_id,
        body,
        as_session: caller(host, args)?,
    };
    let cell = host
        .client
        .send_command::<SendMessage, SendMessageResult>(&cmd);
    let result = await_command(cell, REQUEST_TIMEOUT)
        .await
        .map_err(ToolError::invalid_params)?;
    // The daemon persists the message to the recipient's inbox BEFORE deciding on a
    // live push, so reaching here (no command error) means it WAS delivered. Say so
    // explicitly: a bare `delivered_live:false` reads as a FAILURE when it only means
    // "went to the inbox, not a live push" — that ambiguity has repeatedly triggered
    // false "marshal is broken" alarms. `delivered_live` stays for compatibility.
    let delivered_live = result.delivered_live;
    Ok(ToolOutcome::Json(json!({
        "message_id": result.message_id.0.as_ref(),
        "to_session_id": result.to_session_id.0.as_ref(),
        "sent_at": result.sent_at,
        "delivered": true,
        "delivery": if delivered_live { "live" } else { "inbox" },
        "delivered_live": delivered_live,
        "note": if delivered_live {
            "Delivered and live-pushed into the recipient's current turn."
        } else {
            "Delivered to the recipient's marshal inbox — they read it on their next \
             turn. delivered_live=false is normal here (the recipient has no live \
             channel open); it is NOT a failure."
        },
    })))
}

async fn broadcast(host: &ToolHost, args: &Value) -> Result<ToolOutcome, ToolError> {
    let to_room = arg_str(args, "to_room", "broadcast: missing `to_room` (room id)")?;
    let body = arg_str(args, "body", "broadcast: missing `body`")?;
    let cmd = BroadcastMessage {
        to_room_id: RoomId(Arc::<str>::from(to_room.as_str())),
        body,
        as_session: caller(host, args)?,
    };
    let cell = host
        .client
        .send_command::<BroadcastMessage, BroadcastMessageResult>(&cmd);
    let result = await_command(cell, REQUEST_TIMEOUT)
        .await
        .map_err(ToolError::invalid_params)?;
    Ok(ToolOutcome::Json(json!(result)))
}

async fn join_room(host: &ToolHost, args: &Value) -> Result<ToolOutcome, ToolError> {
    let name = arg_str(args, "name", "join_room: missing `name`")?;
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let cmd = JoinRoom {
        name,
        description,
        as_session: caller(host, args)?,
    };
    let cell = host.client.send_command::<JoinRoom, JoinRoomResult>(&cmd);
    let result = await_command(cell, REQUEST_TIMEOUT)
        .await
        .map_err(ToolError::invalid_params)?;
    Ok(ToolOutcome::Json(json!(result)))
}

async fn leave_room(host: &ToolHost, args: &Value) -> Result<ToolOutcome, ToolError> {
    let room = arg_str(args, "room", "leave_room: missing `room` (id or name)")?;
    let cmd = LeaveRoom {
        room,
        as_session: caller(host, args)?,
    };
    let cell = host.client.send_command::<LeaveRoom, LeaveRoomResult>(&cmd);
    let result = await_command(cell, REQUEST_TIMEOUT)
        .await
        .map_err(ToolError::invalid_params)?;
    Ok(ToolOutcome::Json(json!(result)))
}

async fn ack_messages(host: &ToolHost, args: &Value) -> Result<ToolOutcome, ToolError> {
    let ids = args
        .get("message_ids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            ToolError::invalid_params("ack_messages: missing `message_ids` (array of ids)")
        })?;
    let message_ids: Vec<MessageId> = ids
        .iter()
        .filter_map(|v| v.as_str())
        .map(|s| MessageId(Arc::<str>::from(s)))
        .collect();
    let cmd = AckMessages {
        message_ids,
        as_session: caller(host, args)?,
    };
    let cell = host
        .client
        .send_command::<AckMessages, AckMessagesResult>(&cmd);
    let result = await_command(cell, REQUEST_TIMEOUT)
        .await
        .map_err(ToolError::invalid_params)?;
    Ok(ToolOutcome::Json(json!(result)))
}

// =============================================================================
// Definitions advertised in the MCP `tools/list` and `resources/list` replies
// =============================================================================

fn schema_object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// Build a write-tool input schema, adding a required `asSession` argument
/// under the Codex harness (where the shim has no connection identity and the
/// agent must name its own session — the id the SessionStart hook injected).
fn write_schema(is_codex: bool, mut properties: Value, required: &[&str]) -> Value {
    let mut req: Vec<String> = required.iter().map(|s| (*s).to_string()).collect();
    if is_codex {
        if let Some(obj) = properties.as_object_mut() {
            obj.insert(
                "asSession".into(),
                json!({
                    "type": "string",
                    "description": "YOUR own marshal session id — copy it from the <marshal_session> block injected at session start. Required so peers see who sent this and can reply to the right session."
                }),
            );
        }
        req.push("asSession".into());
    }
    let req_refs: Vec<&str> = req.iter().map(String::as_str).collect();
    schema_object(properties, &req_refs)
}

pub fn tools_def(is_codex: bool) -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "set_status".into(),
            description: "Set this session's free-form status text (the `current_task` field on the roster).".into(),
            input_schema: write_schema(is_codex,
                json!({
                    "text": { "type": "string", "description": "Free-form status text. Empty string clears." }
                }),
                &["text"],
            ),
        },
        ToolDef {
            name: "send_message".into(),
            description: "Direct send to a peer agent, or to a human via their agent. Address by nickname (the `swift-falcon` shown in their statusline / marshal://roster), a session_id, a session_id prefix, or — to reach the human rather than one specific agent — their operator identity (the email on their roster row, e.g. `max@lucid.rocks`, optionally `op:`/`human:`-prefixed), which routes to whichever of their agents is currently most active. Resolved against the live roster; an ambiguous/unknown token returns an error listing the candidates. Make the body explicit: what you need, any action you expect, and whether you want a reply/decision or are just informing (FYI) — vague or unaddressed asks get misread, ignored, or duplicated. On success `delivered` is always true; `delivery` is `live` (pushed into the recipient's active turn) or `inbox` (they read it on their next turn) — `inbox`/`delivered_live:false` is normal, NOT a failure.".into(),
            input_schema: write_schema(is_codex,
                json!({
                    "to":   { "type": "string", "description": "Recipient: a nickname (e.g. `swift-falcon`), full `session_id`, session_id prefix, or an operator identity/email (e.g. `max@lucid.rocks`) to reach the human via their most-active agent — all from marshal://roster." },
                    "body": { "type": "string", "description": "Message body." }
                }),
                &["to", "body"],
            ),
        },
        ToolDef {
            name: "broadcast".into(),
            description: "Ambient fan-out to a room — the message is addressed to the room and surfaced there (marshal UI / `marshal://messages room=…`), NOT injected into members' turns, so it never hijacks anyone's context. To pull a specific peer in, @mention them in the body (`@swift-falcon`, or `@max@lucid.rocks` to reach a human): each resolved handle ALSO gets a real direct message (inbox + live push), even if they aren't in the room — so @mention is a genuine interrupt, not casual chat syntax: use it only for a peer you specifically need to pull in. Returns delivered + the resolved `mentioned` list; errors if the room has no other members.".into(),
            input_schema: write_schema(is_codex,
                json!({
                    "to_room": { "type": "string", "description": "Room id from marshal://rooms — `everyone`, `op:*`, `project:*`, or any ad-hoc room id." },
                    "body":    { "type": "string", "description": "Message body." }
                }),
                &["to_room", "body"],
            ),
        },
        ToolDef {
            name: "join_room".into(),
            description: "Create or join an ad-hoc room. Reserved prefixes (everyone, host:, op:, project:) are blocked — those auto-rooms are managed by the daemon. Returns whether this call created the room and whether it added a new membership row.".into(),
            input_schema: write_schema(is_codex,
                json!({
                    "name":        { "type": "string", "description": "Display name; slugified into the room id (e.g. \"Frontend Redesign\" -> frontend-redesign)." },
                    "description": { "type": "string", "description": "Optional human-readable purpose." }
                }),
                &["name"],
            ),
        },
        ToolDef {
            name: "leave_room".into(),
            description: "Leave an ad-hoc room. Errors on auto-rooms (their membership is derived from your session's identity).".into(),
            input_schema: write_schema(is_codex,
                json!({
                    "room": { "type": "string", "description": "Room id (preferred) or original name." }
                }),
                &["room"],
            ),
        },
        ToolDef {
            name: "ack_messages".into(),
            description: "Mark message ids as read for this session. Idempotent. Returns counts of newly-acked vs already-acked.".into(),
            input_schema: write_schema(is_codex,
                json!({
                    "message_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Message ids returned by marshal://messages."
                    }
                }),
                &["message_ids"],
            ),
        },
    ]
}

pub fn resources_def(is_codex: bool) -> Vec<ResourceDef> {
    // Under Codex the shim isn't told which session it serves, so the two
    // caller-relative reads take the agent's own id as a `?asSession=` query
    // param (the id from the <marshal_session> block). Claude resolves the
    // caller from its WS connection, so this note is Codex-only. roster/rooms
    // need no caller and are unaffected.
    let codex_as = if is_codex {
        " Under Codex, pass your own id as `?asSession=<id>` (from the <marshal_session> block) — without it whoami can't name you and messages is rejected."
    } else {
        ""
    };
    vec![
        ResourceDef {
            uri: "marshal://whoami".into(),
            name: "whoami".into(),
            description: format!("This session's id, pid, cwd, operator, and host info.{codex_as}"),
            mime_type: "application/json".into(),
        },
        ResourceDef {
            uri: "marshal://roster".into(),
            name: "roster".into(),
            description: "Every live session with its id, cwd, git branch, status, operator, host, project, and room memberships.".into(),
            mime_type: "application/json".into(),
        },
        ResourceDef {
            uri: "marshal://rooms".into(),
            name: "rooms".into(),
            description: "Every room (auto and ad-hoc) with its members.".into(),
            mime_type: "application/json".into(),
        },
        ResourceDef {
            uri: "marshal://messages".into(),
            name: "messages".into(),
            description: format!("Message history. Query params: room=ID, from=SID, to_session=SID, inbox=true, sent=true, unread=true, since=MILLIS, limit=N. Default returns the 50 most recent messages visible to you (sent, direct-recipient, or via room membership).{codex_as}"),
            mime_type: "application/json".into(),
        },
    ]
}

// =============================================================================
// Helpers
// =============================================================================

fn arg_str(args: &Value, key: &str, missing_msg: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| ToolError::invalid_params(missing_msg))
}

fn parse_bool(s: &str) -> bool {
    matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "y")
}

fn json_resource(uri: &str, value: Value) -> ResourceContent {
    ResourceContent {
        uri: uri.to_string(),
        mime_type: "application/json".to_string(),
        text: serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
    }
}

/// Parse an MCP resource URI like `marshal://messages?inbox=true&unread=true`
/// into its path + query components. Rejects URIs without the
/// `marshal://` scheme so we don't accidentally serve resources from
/// other schemes a future host might invent.
struct ParsedUri {
    path: String,
    query: std::collections::HashMap<String, String>,
}

impl ParsedUri {
    fn parse(uri: &str) -> Result<Self, ResourceError> {
        let rest = uri
            .strip_prefix("marshal://")
            .ok_or_else(|| ResourceError {
                code: 0,
                message: format!(
                    "unsupported resource scheme in '{uri}'; marshal serves marshal:// URIs only",
                ),
                data: None,
            })?;
        let (path, query_str) = match rest.split_once('?') {
            Some((p, q)) => (p, q),
            None => (rest, ""),
        };
        let mut query = std::collections::HashMap::new();
        if !query_str.is_empty() {
            for pair in query_str.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    query.insert(url_decode(k).into_owned(), url_decode(v).into_owned());
                } else if !pair.is_empty() {
                    query.insert(url_decode(pair).into_owned(), String::new());
                }
            }
        }
        Ok(Self {
            path: path.to_string(),
            query,
        })
    }
}

/// Minimal percent-decoding good enough for our query strings (we only
/// ever produce simple ascii values, but URIs may pass through hosts
/// that re-encode special chars). Falls back to the raw string on any
/// malformed escape.
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

/// Wait for a `send_command` Cell to settle.
async fn await_command<R>(
    cell: Cell<Option<Result<R, String>>, CellImmutable>,
    timeout: Duration,
) -> Result<R, String>
where
    R: Clone + std::fmt::Debug + PartialEq + Send + Sync + 'static,
{
    if let Some(result) = cell.get() {
        return result;
    }
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<R, String>>();
    let tx_slot = Arc::new(Mutex::new(Some(tx)));
    let tx_for_sub = Arc::clone(&tx_slot);
    let guard = cell.subscribe(move |signal| {
        if let Signal::Value(opt) = signal
            && let Some(result) = (**opt).clone()
            && let Ok(mut slot) = tx_for_sub.lock()
            && let Some(tx) = slot.take()
        {
            let _ = tx.send(result);
        }
    });
    cell.own(guard);
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("command response handler dropped".to_string()),
        Err(_) => Err(format!(
            "command timed out after {} ms (daemon unresponsive?)",
            timeout.as_millis()
        )),
    }
}
