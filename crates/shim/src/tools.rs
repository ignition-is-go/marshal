//! MCP tool dispatch — translates each tool call into MykoClient operations.

use crate::mcp::{METHOD_NOT_FOUND, Notifier, ToolError, ToolFuture, ToolHandler, ToolOutcome};
use hyphae::{Cell, CellImmutable, Gettable, Signal, Watchable};
use marshal_entities::{SendMessage, SendMessageResult, Session, SessionId, SetSessionCurrentTask};
use myko::client::MykoClient;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct ToolHost {
    pub client: Arc<MykoClient>,
    pub session_id: SessionId,
    pub nickname: String,
    pub pid: u32,
    pub cwd: String,
    /// The shim's local copy of its Session entity. Mutations (set_status)
    /// update this and re-emit a SET event so the server's view stays in
    /// sync.
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
                        .ok_or_else(|| {
                            ToolError::invalid_params(
                                "send_message: missing `to` (session id from `roster`)",
                            )
                        })?
                        .to_string();
                    let body = args
                        .get("body")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::invalid_params("send_message: missing `body`"))?
                        .to_string();

                    let cmd = SendMessage {
                        to_session_id: SessionId(Arc::<str>::from(to.as_str())),
                        body,
                    };
                    let cell = host
                        .client
                        .send_command::<SendMessage, SendMessageResult>(&cmd);
                    let result = await_command_response(cell, Duration::from_secs(10))
                        .await
                        .map_err(|e| ToolError::invalid_params(e))?;

                    Ok(ToolOutcome::Json(json!({
                        "message_id": result.message_id.0.as_ref(),
                        "to_session_id": cmd.to_session_id.0.as_ref(),
                        "to_nick": result.to_nick,
                        "sent_at": result.sent_at,
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

/// Wait for a `send_command` result Cell to settle.
///
/// The Cell starts at `None` and transitions to `Some(Ok(_))` or
/// `Some(Err(_))` when the daemon's RPC reply arrives. We subscribe and
/// forward the first settled value through a one-shot channel. A timeout
/// guards against a permanently-pending Cell (e.g. daemon crashed
/// mid-request).
async fn await_command_response<R>(
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
