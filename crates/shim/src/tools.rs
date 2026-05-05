//! MCP tool dispatch — translates each tool call into MykoClient operations.

use crate::mcp::{
    INVALID_PARAMS, METHOD_NOT_FOUND, Notifier, ToolError, ToolFuture, ToolHandler, ToolOutcome,
};
use chrono::Utc;
use entities::{Message, MessageId, Session, SessionId, SetSessionCurrentTask};
use hyphae::{Cell, CellImmutable, Gettable};
use myko::{
    client::MykoClient,
    wire::{MEvent, MEventType},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub struct ToolHost {
    pub client: Arc<MykoClient>,
    pub session_id: SessionId,
    pub nickname: String,
    pub pid: u32,
    pub cwd: String,
    /// The shim's local copy of its Session entity. Mutations (set_status)
    /// update this and re-emit a SET event so the server's view stays in
    /// sync. The `role` field is owned by the server (assigned by the
    /// daemon's classifier), so the shim never writes to it.
    pub session: Arc<Mutex<Session>>,
    /// Long-lived `GetAllSessions` subscription. We hold this so the cell
    /// is kept warm across tool calls — otherwise creating it inside
    /// `roster()` would race the server's first response and snapshot an
    /// empty Vec.
    pub sessions_cell: Cell<Vec<Arc<Session>>, CellImmutable>,
}

pub struct CoordHandler {
    pub host: Arc<ToolHost>,
}

impl ToolHandler for CoordHandler {
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        _notifier: &'a Notifier,
    ) -> ToolFuture<'a> {
        let host = Arc::clone(&self.host);
        Box::pin(async move {
            match name {
                "whoami" => Ok(ToolOutcome::Json(json!({
                    "session_id": host.session_id.0.as_ref(),
                    "nickname": host.nickname,
                    "pid": host.pid,
                    "cwd": host.cwd,
                }))),

                "set_status" => {
                    let text = args
                        .get("text")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::invalid_params("set_status: missing `text`"))?
                        .to_string();
                    // Dispatch the auto-generated `SetSessionCurrentTask`
                    // setter command rather than emitting a full Session
                    // SET. A full SET would include `role: None` (the
                    // shim's local default — the shim is intentionally
                    // not the source of truth on roles), which would
                    // clobber whatever role the daemon's classifier had
                    // already assigned. The setter command is a partial
                    // update server-side, so other fields (role,
                    // client_id, connected_at) are preserved.
                    let new_task = if text.is_empty() {
                        None
                    } else {
                        Some(Arc::<str>::from(text.as_str()))
                    };
                    let _resp = host.client.send_command::<SetSessionCurrentTask, ()>(
                        &SetSessionCurrentTask {
                            id: host.session_id.clone(),
                            current_task: new_task,
                        },
                    );
                    // Keep our local mirror in sync so subsequent
                    // reconnect re-SETs (which still send the full
                    // entity, by design) include the latest text.
                    {
                        let mut sess = host.session.lock().unwrap();
                        sess.current_task = if text.is_empty() { None } else { Some(text) };
                    }
                    Ok(ToolOutcome::Json(json!({ "ok": true })))
                }

                "roster" => {
                    let sessions: Vec<Arc<Session>> = host.sessions_cell.get();
                    let me = host.session_id.0.as_ref();
                    let view: Vec<Value> = sessions
                        .iter()
                        .map(|s| {
                            json!({
                                "session_id": s.id.0.as_ref(),
                                "is_self": s.id.0.as_ref() == me,
                                "nickname": s.nickname,
                                "pid": s.pid,
                                "cwd": s.cwd,
                                "git_branch": s.git_branch,
                                "current_task": s.current_task,
                                "role": s.role,
                                "connected_at": s.connected_at,
                            })
                        })
                        .collect();
                    Ok(ToolOutcome::Json(json!({ "sessions": view })))
                }

                "send_message" => {
                    let to = args
                        .get("to")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::invalid_params("send_message: missing `to`"))?
                        .to_string();
                    let body = args
                        .get("body")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::invalid_params("send_message: missing `body`"))?
                        .to_string();

                    let target = resolve_recipient(&host.sessions_cell, &to).ok_or_else(|| {
                        ToolError::invalid_params(format!("no live session matches '{to}'"))
                    })?;

                    let now = Utc::now().timestamp_millis();
                    let msg = Message {
                        id: MessageId(Arc::from(Uuid::new_v4().to_string())),
                        from_session_id: host.session_id.clone(),
                        from_nick: host.nickname.clone(),
                        to_session_id: target.id.clone(),
                        to_nick: target.nickname.clone(),
                        body,
                        sent_at: now,
                        read_at: None,
                    };
                    let event = MEvent::from_item(
                        &msg,
                        MEventType::SET,
                        &Uuid::new_v4().to_string(),
                    );
                    host.client
                        .send_event(event)
                        .map_err(|e| ToolError::internal(format!("send_event: {e}")))?;

                    Ok(ToolOutcome::Json(json!({
                        "message_id": msg.id.0.as_ref(),
                        "to_session_id": target.id.0.as_ref(),
                        "to_nick": target.nickname,
                        "sent_at": now,
                    })))
                }

                other => Err(ToolError {
                    code: METHOD_NOT_FOUND,
                    message: format!("unknown tool: {other}"),
                    data: None,
                }),
            }
        })
    }
}

#[derive(Clone, Debug)]
struct ResolvedRecipient {
    id: SessionId,
    nickname: String,
}

/// Look up a session by id or nickname. Returns None if no live session
/// matches; if multiple match the same nickname, returns one
/// nondeterministically (caller can be more specific by passing the id).
fn resolve_recipient(
    sessions_cell: &Cell<Vec<Arc<Session>>, CellImmutable>,
    target: &str,
) -> Option<ResolvedRecipient> {
    let sessions: Vec<Arc<Session>> = sessions_cell.get();

    if let Some(s) = sessions.iter().find(|s| s.id.0.as_ref() == target) {
        return Some(ResolvedRecipient {
            id: s.id.clone(),
            nickname: s.nickname.clone(),
        });
    }
    if let Some(s) = sessions.iter().find(|s| s.nickname == target) {
        return Some(ResolvedRecipient {
            id: s.id.clone(),
            nickname: s.nickname.clone(),
        });
    }
    None
}

#[allow(dead_code)]
fn _invalid_params_marker() -> ToolError {
    ToolError {
        code: INVALID_PARAMS,
        message: String::new(),
        data: None,
    }
}

// MessageId is referenced via Message construction above; keep the unused
// import suppressed.
#[allow(dead_code)]
type _MessageIdAlias = MessageId;
