//! claude-coord-shim — stdio MCP server backed by a MykoClient.
//!
//! On startup the shim:
//! 1. connects MykoClient to MYKO_ADDRESS (default ws://localhost:6155),
//! 2. SETs a `Session` entity describing this Claude session,
//! 3. registers `on_command::<NotifyChannel>` so daemon-pushed notifications
//!    are forwarded as `notifications/claude/channel` MCP events,
//! 4. serves stdio MCP with a curated tool surface backed by the MykoClient.

mod mcp;
mod tools;

use anyhow::{Context, Result};
use chrono::Utc;
use entities::{GetAllSessions, NotifyChannel, Session, SessionId};
use hyphae::Watchable;
use mcp::{ServerConfig, ToolDef};
use myko::{
    client::{ConnectionStatus, MykoClient},
    core::item::Eventable,
    wire::{MEvent, MEventType},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use uuid::Uuid;

const DEFAULT_MYKO_ADDRESS: &str = "ws://localhost:6155";

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    entities::link();

    let myko_address =
        std::env::var("MYKO_ADDRESS").unwrap_or_else(|_| DEFAULT_MYKO_ADDRESS.to_string());

    log::info!("[claude-coord-shim] connecting to {myko_address}");

    let client = Arc::new(MykoClient::new());

    // Track connected status so we can defer the initial Session SET until
    // the WebSocket actually opens.
    let (connected_tx, mut connected_rx) = watch::channel(false);
    let conn_guard = client.connection_status().subscribe(move |signal| {
        if let hyphae::Signal::Value(status) = signal {
            match &**status {
                ConnectionStatus::Connected(addr) => {
                    log::info!("[claude-coord-shim] connected to {addr}");
                    let _ = connected_tx.send(true);
                }
                ConnectionStatus::Disconnected => {
                    log::warn!("[claude-coord-shim] disconnected");
                    let _ = connected_tx.send(false);
                }
                _ => {}
            }
        }
    });
    client.connection_status().own(conn_guard);
    client.set_address(Some(myko_address));

    // Local session metadata.
    let cwd = std::env::current_dir()
        .context("getting cwd")?
        .display()
        .to_string();
    let pid = std::process::id();
    let nickname = std::path::Path::new(&cwd)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("session")
        .to_string();
    let git_branch = detect_git_branch(&cwd);
    let session_id = SessionId(Arc::from(Uuid::new_v4().to_string()));

    let session = Session {
        id: session_id.clone(),
        client_id: None,
        nickname: nickname.clone(),
        pid,
        cwd: cwd.clone(),
        git_branch: git_branch.clone(),
        current_task: None,
        role: None,
        connected_at: Utc::now().timestamp_millis(),
    };
    let session = Arc::new(Mutex::new(session));

    // Wait for the WebSocket to open before we SET our Session — otherwise
    // the initial event would be queued until reconnect, but the daemon
    // would have no record of us until the first reconnect tick.
    while !*connected_rx.borrow() {
        connected_rx
            .changed()
            .await
            .context("connection_status channel closed")?;
    }

    set_session(&client, &session.lock().unwrap())?;

    // Open the long-lived sessions subscription before we start serving MCP.
    // The cell starts empty and fills in once the server responds; tools
    // that snapshot it (roster, send_message recipient resolution) need it
    // hot, not freshly-opened on every call.
    let sessions_cell = client.watch_query::<GetAllSessions>(GetAllSessions {});

    let host = Arc::new(tools::ToolHost {
        client: Arc::clone(&client),
        session_id: session_id.clone(),
        nickname: nickname.clone(),
        pid,
        cwd: cwd.clone(),
        session: Arc::clone(&session),
        sessions_cell,
    });

    let handler = Arc::new(tools::CoordHandler { host });

    let config = ServerConfig {
        name: "claude-coord-shim".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        instructions: format!(
            "Coordinate with sibling Claude sessions via the claude-coord daemon. \
             Tools: whoami, set_status, set_role, roster, send_message. You are \
             session '{nickname}' (id {}). New messages addressed to you arrive \
             as notifications/claude/channel events.",
            session_id.0
        ),
        tools: tools_def(),
    };

    let on_initialized_client = Arc::clone(&client);
    mcp::serve_stdio(config, handler, move |notifier| {
        let guard = on_initialized_client.on_command::<NotifyChannel, _>(
            move |cmd, _responder| {
                notifier.channel(cmd.content, cmd.meta);
            },
        );
        Box::leak(Box::new(guard));
        log::info!("[claude-coord-shim] NotifyChannel handler registered");
    })
    .await
}

/// SET our Session entity so it appears on the roster and the daemon can
/// route messages to us. The daemon auto-populates the `client_id` field
/// from the WebSocket connection.
fn set_session(client: &MykoClient, session: &Session) -> Result<()> {
    let event = MEvent::from_item(session, MEventType::SET, &Uuid::new_v4().to_string());
    client
        .send_event(event)
        .map_err(|e| anyhow::anyhow!("send_event failed: {e}"))?;
    Ok(())
}

fn init_logging() {
    let mut b = env_logger::Builder::from_default_env();
    if std::env::var("RUST_LOG").is_err() {
        b.filter_level(log::LevelFilter::Info);
    }
    b.target(env_logger::Target::Stderr).init();
}

fn detect_git_branch(cwd: &str) -> Option<String> {
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

fn schema_object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn tools_def() -> Vec<ToolDef> {
    let empty = schema_object(json!({}), &[]);
    vec![
        ToolDef {
            name: "whoami".into(),
            description: "Return this session's id, nickname, pid, and cwd.".into(),
            input_schema: empty.clone(),
        },
        ToolDef {
            name: "roster".into(),
            description: "List all live coord sessions (snapshot of the daemon's session entities).".into(),
            input_schema: empty,
        },
        ToolDef {
            name: "set_status".into(),
            description: "Set this session's free-form status text (the `current_task` field on the roster).".into(),
            input_schema: schema_object(
                json!({
                    "text": {
                        "type": "string",
                        "description": "Free-form status text. Empty string clears."
                    }
                }),
                &["text"],
            ),
        },
        ToolDef {
            name: "set_role".into(),
            description: "Assign this session a role and receive behavioral instructions to follow going forward. The tool result is a directive — read and follow it before doing anything else.".into(),
            input_schema: schema_object(
                json!({
                    "role": {
                        "type": "string",
                        "description": "Role name. Built-ins: 'worker', 'task_distributor' (alias 'distributor'), 'communicator'. Empty string clears."
                    }
                }),
                &["role"],
            ),
        },
        ToolDef {
            name: "send_message".into(),
            description: "Send a message to another session by id or nickname.".into(),
            input_schema: schema_object(
                json!({
                    "to": {
                        "type": "string",
                        "description": "Recipient session id (uuid) or nickname."
                    },
                    "body": {
                        "type": "string",
                        "description": "Message body."
                    }
                }),
                &["to", "body"],
            ),
        },
    ]
}
