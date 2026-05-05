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
use entities::{GetAllSessions, NotifyChannel, Session, SessionId, role_instructions};
use hyphae::{Signal, Watchable};
use mcp::{ServerConfig, ToolDef};
use myko::{
    client::{ConnectionStatus, MykoClient},
    wire::{MEvent, MEventType},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
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

    // Register on_command::<NotifyChannel> *before* we connect, so daemon-
    // pushed notifications that arrive between Session-SET and MCP-init are
    // buffered into a channel rather than dropped. The drain task that
    // forwards buffered notifications onto stdout is spawned later, once
    // the MCP `Notifier` exists.
    let (notify_tx, notify_rx) = mpsc::unbounded_channel::<NotifyChannel>();
    let notify_tx_oncmd = notify_tx.clone();
    let notify_guard = client.on_command::<NotifyChannel, _>(move |cmd, _responder| {
        let _ = notify_tx_oncmd.send(cmd);
    });
    Box::leak(Box::new(notify_guard));

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

    // Open every query/subscription BEFORE we ask the client to connect.
    // Once `set_address` returns, the WS handshake races the rest of this
    // setup; if the handshake wins, the `Connected` callback fires and
    // emits our first Session SET, which kicks off classification on the
    // daemon and produces a `NotifyChannel` role assignment. If our
    // queries / on_command handlers aren't all registered by then, the
    // first round of state can be missed (e.g. the role notification
    // arrives before there's a place to receive it). Registering them
    // pre-connect closes that window.
    //
    // The long-lived `GetAllSessions` cell — kept hot so tools that
    // snapshot it (roster, send_message recipient resolution) don't
    // race the server's first response.
    let sessions_cell = client.watch_query::<GetAllSessions>(GetAllSessions {});

    // Local role watcher: surface our own role to Claude as a
    // `NotifyChannel`-style notification whenever it changes. The daemon
    // also pushes `NotifyChannel` for role changes via
    // `RoleChangeNotifySaga`, but shim-side detection is a resilient
    // backup: it doesn't depend on the daemon-side
    // `client_registry.send_command_request_to(...)` resolving to a live
    // writer at the exact moment classification fires. We dedupe on
    // last-seen role so the daemon push and this push don't both surface
    // the same change to Claude.
    let last_role: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let last_role_watch = Arc::clone(&last_role);
    let session_id_watch = session_id.clone();
    let notify_tx_watch = notify_tx.clone();
    let role_guard = sessions_cell.subscribe(move |signal| {
        let Signal::Value(value) = signal else { return };
        let Some(my_session) = value
            .iter()
            .find(|s| s.id.0.as_ref() == session_id_watch.0.as_ref())
        else {
            return;
        };
        let new_role = my_session.role.clone();
        {
            let mut last = last_role_watch.lock().unwrap();
            if *last == new_role {
                return;
            }
            *last = new_role.clone();
        }
        let canonical = new_role
            .as_deref()
            .map(role_instructions::canonicalize)
            .unwrap_or_default();
        let instructions = role_instructions::instructions(&canonical);
        let content = if canonical.is_empty() {
            format!("claude-coord: your role has been cleared.\n\n{instructions}")
        } else {
            format!(
                "claude-coord: your role was set to '{canonical}'.\n\n{instructions}"
            )
        };
        let cmd = NotifyChannel {
            content,
            meta: serde_json::json!({
                "source": "claude-coord",
                "kind": "role_changed",
                "role": canonical,
            }),
        };
        let _ = notify_tx_watch.send(cmd);
    });
    sessions_cell.own(role_guard);

    // Re-SET our Session on every connect. The daemon holds session state
    // in-memory, so a daemon restart drops every roster entry; we have to
    // re-publish on reconnect or peers can't see us anymore. This also
    // handles the initial connection — the subscriber fires synchronously
    // the moment the WebSocket opens.
    let session_for_resend = Arc::clone(&session);
    let client_for_resend = Arc::clone(&client);
    let conn_guard = client.connection_status().subscribe(move |signal| {
        if let hyphae::Signal::Value(status) = signal {
            match &**status {
                ConnectionStatus::Connected(addr) => {
                    log::info!("[claude-coord-shim] connected to {addr} — (re)sending session");
                    let snapshot = session_for_resend.lock().unwrap().clone();
                    if let Err(e) = emit_session_set(&client_for_resend, &snapshot) {
                        log::warn!("[claude-coord-shim] re-SET on connect failed: {e}");
                    }
                }
                ConnectionStatus::Disconnected => {
                    log::warn!("[claude-coord-shim] disconnected");
                }
                _ => {}
            }
        }
    });
    client.connection_status().own(conn_guard);

    // All queries / handlers / connection subscribers are registered.
    // Now it's safe to start the WS handshake.
    client.set_address(Some(myko_address));

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
             Tools: whoami, set_status, roster, send_message. You are \
             session '{nickname}' (id {}). New messages addressed to you arrive \
             as notifications/claude/channel events. Your role on the roster is \
             assigned automatically by the daemon based on your cwd — there is \
             no client-side way to set or change it.",
            session_id.0
        ),
        tools: tools_def(),
    };

    let notify_rx = Mutex::new(Some(notify_rx));
    mcp::serve_stdio(config, handler, move |notifier| {
        // Spawn a task that drains the NotifyChannel buffer and emits each
        // one onto stdout via the MCP writer. The buffer accumulated any
        // notifications that fired before MCP init (the daemon's role
        // assignment for the very first Session SET, in particular).
        if let Some(mut rx) = notify_rx.lock().ok().and_then(|mut g| g.take()) {
            tokio::spawn(async move {
                while let Some(cmd) = rx.recv().await {
                    notifier.channel(cmd.content, cmd.meta);
                }
            });
            log::info!("[claude-coord-shim] notification drain task started");
        }
    })
    .await
}

/// SET our Session entity. Used both on initial connect and on every
/// subsequent reconnect (the daemon's in-memory store loses everything
/// when it restarts, so we have to re-publish or peers can't see us).
/// The server auto-populates `client_id` from the WS connection.
fn emit_session_set(client: &MykoClient, session: &Session) -> Result<()> {
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
