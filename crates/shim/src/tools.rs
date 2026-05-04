//! MCP tool dispatch — translates each tool call into MykoClient operations.

use crate::mcp::{
    INVALID_PARAMS, METHOD_NOT_FOUND, Notifier, ToolError, ToolFuture, ToolHandler, ToolOutcome,
};
use crate::role_instructions;
use chrono::Utc;
use entities::{GetAllSessions, Message, MessageId, Session, SessionId};
#[allow(unused_imports)]
use entities::MessageId as _MessageId;
use hyphae::{Gettable, Watchable};
use myko::{
    client::MykoClient,
    core::item::Eventable,
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
    /// The shim's local copy of its Session entity. Mutations (set_status,
    /// set_role) update this and re-emit a SET event so the server's view
    /// stays in sync.
    pub session: Arc<Mutex<Session>>,
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

                "set_role" => {
                    let role = args
                        .get("role")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::invalid_params("set_role: missing `role`"))?
                        .to_string();
                    let canonical = role_instructions::canonicalize(&role);
                    let instructions = role_instructions::instructions(&canonical);

                    // Mutate our local session and re-emit. Empty string clears.
                    {
                        let mut sess = host.session.lock().unwrap();
                        sess.role = if canonical.is_empty() {
                            None
                        } else {
                            Some(canonical.clone())
                        };
                        emit_session_set(&host.client, &sess)
                            .map_err(|e| ToolError::internal(format!("set_role: {e}")))?;
                    }

                    Ok(ToolOutcome::Json(json!({
                        "role": canonical,
                        "instructions": instructions,
                    })))
                }

                "set_status" => {
                    let text = args
                        .get("text")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::invalid_params("set_status: missing `text`"))?
                        .to_string();
                    {
                        let mut sess = host.session.lock().unwrap();
                        sess.current_task = if text.is_empty() { None } else { Some(text) };
                        emit_session_set(&host.client, &sess)
                            .map_err(|e| ToolError::internal(format!("set_status: {e}")))?;
                    }
                    Ok(ToolOutcome::Json(json!({ "ok": true })))
                }

                "roster" => {
                    let sessions_cell =
                        host.client.watch_query::<GetAllSessions>(GetAllSessions {});
                    let sessions: Vec<Arc<Session>> = sessions_cell.get();
                    // Drop the cell so the underlying query subscription goes away.
                    drop(sessions_cell);

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

                    let target = resolve_recipient(&host.client, &to).ok_or_else(|| {
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
fn resolve_recipient(client: &MykoClient, target: &str) -> Option<ResolvedRecipient> {
    let cell = client.watch_query::<GetAllSessions>(GetAllSessions {});
    let sessions: Vec<Arc<Session>> = cell.get();

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

fn emit_session_set(client: &MykoClient, session: &Session) -> Result<(), String> {
    let event = MEvent::from_item(session, MEventType::SET, &Uuid::new_v4().to_string());
    client.send_event(event)
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
