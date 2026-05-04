//! claude-coord-shim — MCP server bridging the host (Claude Code) to the daemon.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use shim::daemon_client::{CallError, DaemonClient, EventMsg};
use shim::mcp::{
    self, Notifier, ServerConfig, ToolDef, ToolError, ToolFuture, ToolHandler, ToolOutcome,
    INVALID_PARAMS, METHOD_NOT_FOUND,
};
use shim::spawn;
use shim::tools::ToolHost;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as TokioMutex};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let socket = socket_path();
    if let Some(parent) = socket.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let daemon_bin = spawn::locate_daemon().context("locating daemon binary")?;
    spawn::ensure_running(&socket, &daemon_bin)
        .await
        .context("starting daemon")?;

    let cwd = std::env::current_dir().context("cwd")?;
    let pid = std::process::id();
    let git_branch = detect_git_branch(&cwd);

    let client = Arc::new(
        DaemonClient::new(socket, pid, cwd.clone(), git_branch)
            .with_auto_respawn(daemon_bin.clone()),
    );
    let events_rx = client.take_events_rx().await;

    // Connect now so we can include current peers in the MCP `instructions`.
    let (session_id, nickname) = match client.ensure_connected().await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(error = ?e, "could not pre-fetch roster from daemon");
            (String::new(), String::new())
        }
    };
    let initial_instructions = build_instructions(&client, &session_id, &nickname).await;

    let host = Arc::new(ToolHost {
        client,
        pid,
        cwd,
    });

    let config = ServerConfig {
        name: "claude-coord-shim".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        instructions: initial_instructions,
        tools: tools_def(),
    };

    let handler = Arc::new(CoordHandler {
        host: Arc::clone(&host),
    });
    let session_id_for_init = session_id.clone();
    let nickname_for_init = nickname.clone();
    let host_for_init = Arc::clone(&host);
    let events_rx_holder = Arc::new(TokioMutex::new(events_rx));

    mcp::serve_stdio(config, handler, move |notifier| {
        spawn_initial_channel(
            notifier.clone(),
            session_id_for_init,
            nickname_for_init,
            host_for_init,
        );
        spawn_event_forwarder(notifier, events_rx_holder);
    })
    .await
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("CLAUDE_COORD_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

fn socket_path() -> std::path::PathBuf {
    if let Some(rd) = std::env::var_os("XDG_RUNTIME_DIR") {
        return std::path::PathBuf::from(rd).join("claude-coord/sock");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return std::path::PathBuf::from(home).join(".local/state/claude-coord/sock");
    }
    std::path::PathBuf::from("/tmp/claude-coord/sock")
}

fn detect_git_branch(cwd: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() || s == "HEAD" {
        None
    } else {
        Some(s.to_string())
    }
}

async fn build_instructions(client: &DaemonClient, session_id: &str, nickname: &str) -> String {
    let base = "Coordinate with sibling Claude sessions running on this machine. \
                Tools: whoami, set_status, roster, send_message, inbox, recent_messages.";

    if session_id.is_empty() {
        return format!(
            "{base}\n\n(Failed to reach the claude-coord daemon — tools may error.)"
        );
    }

    let header = format!(
        "{base}\n\nYou are session '{nickname}' (id {session_id}). When asked who else is around, \
         use `roster`; messages from peers arrive in `inbox`. The host will deliver new-message \
         and agent-joined notifications via `notifications/claude/channel`."
    );

    match client.call::<proto::rpc::RosterResult>("roster", json!({})).await {
        Ok(r) => {
            let peers: Vec<&proto::messages::SessionInfo> =
                r.sessions.iter().filter(|s| !s.is_self).collect();
            if peers.is_empty() {
                format!("{header}\n\nNo other sessions are connected right now.")
            } else {
                let mut s = format!("{header}\n\nLive peers ({}):", peers.len());
                for p in peers {
                    s.push_str(&format!(
                        "\n  - {} (id {}) cwd={}",
                        p.nickname,
                        p.session_id,
                        p.cwd.display()
                    ));
                    if let Some(t) = &p.current_task {
                        s.push_str(&format!(" — {t}"));
                    }
                }
                s
            }
        }
        Err(e) => format!("{header}\n\n(Could not fetch roster: {e})"),
    }
}

fn schema_object(properties: Value, required: &[&str]) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("type".into(), Value::String("object".into()));
    map.insert("properties".into(), properties);
    if !required.is_empty() {
        map.insert(
            "required".into(),
            Value::Array(required.iter().map(|s| Value::String((*s).into())).collect()),
        );
    }
    map.insert("additionalProperties".into(), Value::Bool(false));
    Value::Object(map)
}

fn tools_def() -> Vec<ToolDef> {
    let empty = schema_object(json!({}), &[]);
    let set_status = schema_object(
        json!({
            "text": {
                "type": "string",
                "description": "Status text (free-form, shown to other sessions)."
            }
        }),
        &["text"],
    );
    let send_message = schema_object(
        json!({
            "to": {
                "type": "string",
                "description": "Recipient session id or nickname."
            },
            "body": {
                "type": "string",
                "description": "Message body (free-form text)."
            }
        }),
        &["to", "body"],
    );
    let inbox = schema_object(
        json!({
            "mark_read": {
                "type": "boolean",
                "description": "If true (default), mark returned messages as read."
            }
        }),
        &[],
    );
    let recent_messages = schema_object(
        json!({
            "limit": {
                "type": "integer",
                "description": "Maximum number of messages to return (default 50)."
            }
        }),
        &[],
    );

    vec![
        ToolDef {
            name: "whoami".into(),
            description: "Return this shim's session id, nickname, pid, and cwd.".into(),
            input_schema: empty.clone(),
        },
        ToolDef {
            name: "set_status".into(),
            description: "Set this session's free-form status text on the roster.".into(),
            input_schema: set_status,
        },
        ToolDef {
            name: "roster".into(),
            description: "List all live coord sessions with their last-known status.".into(),
            input_schema: empty,
        },
        ToolDef {
            name: "send_message".into(),
            description: "Send a message to another session by id or nickname.".into(),
            input_schema: send_message,
        },
        ToolDef {
            name: "inbox".into(),
            description: "Fetch unread messages addressed to this session.".into(),
            input_schema: inbox,
        },
        ToolDef {
            name: "recent_messages".into(),
            description: "Fetch the most recent messages addressed to this session.".into(),
            input_schema: recent_messages,
        },
    ]
}

struct CoordHandler {
    host: Arc<ToolHost>,
}

impl ToolHandler for CoordHandler {
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        _notifier: &'a Notifier,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            match name {
                "whoami" => {
                    let r = self.host.whoami().await.map_err(map_call_err)?;
                    Ok(ToolOutcome::Json(serde_json::to_value(r).unwrap()))
                }
                "set_status" => {
                    let text = args
                        .get("text")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::invalid_params("set_status: missing `text`"))?
                        .to_string();
                    let r = self.host.set_status(text).await.map_err(map_call_err)?;
                    Ok(ToolOutcome::Json(serde_json::to_value(r).unwrap()))
                }
                "roster" => {
                    let r = self.host.roster().await.map_err(map_call_err)?;
                    Ok(ToolOutcome::Json(serde_json::to_value(r).unwrap()))
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
                    let r = self
                        .host
                        .send_message(to, body)
                        .await
                        .map_err(map_call_err)?;
                    Ok(ToolOutcome::Json(serde_json::to_value(r).unwrap()))
                }
                "inbox" => {
                    let mark_read = args
                        .get("mark_read")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    let r = self.host.inbox(mark_read).await.map_err(map_call_err)?;
                    Ok(ToolOutcome::Json(serde_json::to_value(r).unwrap()))
                }
                "recent_messages" => {
                    let limit = args
                        .get("limit")
                        .and_then(|v| v.as_u64())
                        .map(|n| n.min(u32::MAX as u64) as u32)
                        .unwrap_or(50);
                    let r = self
                        .host
                        .recent_messages(limit)
                        .await
                        .map_err(map_call_err)?;
                    Ok(ToolOutcome::Json(serde_json::to_value(r).unwrap()))
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

fn map_call_err(e: CallError) -> ToolError {
    match e {
        CallError::Disconnected => ToolError::internal("daemon disconnected"),
        CallError::Rpc { code, message } => ToolError {
            code: INVALID_PARAMS,
            message: format!("daemon: {message}"),
            data: Some(json!({"code": format!("{code:?}")})),
        },
        CallError::Other(e) => ToolError::internal(format!("internal: {e}")),
    }
}

/// Send a one-shot `notifications/claude/channel` welcoming the user/agent and
/// listing live peers, immediately after the client signals `initialized`.
fn spawn_initial_channel(
    notifier: Notifier,
    session_id: String,
    nickname: String,
    host: Arc<ToolHost>,
) {
    tokio::spawn(async move {
        let roster = match host.roster().await {
            Ok(r) => r,
            Err(_) => return,
        };
        let peers: Vec<_> = roster
            .sessions
            .into_iter()
            .filter(|s| !s.is_self)
            .map(|s| {
                json!({
                    "session_id": s.session_id,
                    "nickname": s.nickname,
                    "cwd": s.cwd,
                    "current_task": s.current_task,
                })
            })
            .collect();

        let mut content = format!(
            "claude-coord: registered as '{nickname}' (id {session_id})."
        );
        if peers.is_empty() {
            content.push_str(" No other sessions are connected.");
        } else {
            content.push_str(&format!(" Live peers ({}):", peers.len()));
            for p in &peers {
                content.push_str(&format!(
                    "\n  - {} (id {})",
                    p["nickname"].as_str().unwrap_or("?"),
                    p["session_id"].as_str().unwrap_or("?"),
                ));
            }
        }

        notifier.channel(
            content,
            json!({
                "source": "claude-coord",
                "kind": "session_started",
                "session_id": session_id,
                "nickname": nickname,
                "peers": peers,
            }),
        );
    });
}

/// Forward each daemon `EventMsg` as a `notifications/claude/channel` so the
/// host wakes the model with a relevant `<channel>` message in context.
fn spawn_event_forwarder(
    notifier: Notifier,
    events_rx_holder: Arc<TokioMutex<Option<mpsc::UnboundedReceiver<EventMsg>>>>,
) {
    tokio::spawn(async move {
        let mut events_rx = match events_rx_holder.lock().await.take() {
            Some(rx) => rx,
            None => {
                tracing::debug!("events_rx already taken; skipping forwarder");
                return;
            }
        };
        while let Some(event) = events_rx.recv().await {
            let content = render_event(&event.kind, &event.payload);
            notifier.channel(
                content,
                json!({
                    "source": "claude-coord",
                    "kind": event.kind,
                    "payload": event.payload,
                }),
            );
        }
    });
}

fn render_event(kind: &str, payload: &Value) -> String {
    match kind {
        "new_message" => format!(
            "claude-coord: new message from '{}': {}",
            payload["from_nick"].as_str().unwrap_or("?"),
            payload["body"].as_str().unwrap_or("")
        ),
        other => format!("claude-coord: event '{other}': {payload}"),
    }
}
