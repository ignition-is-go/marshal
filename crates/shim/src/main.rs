//! claude-coord-shim — stdio MCP server backed by a MykoClient.
//!
//! Architecture:
//! - `mcp.rs` owns stdio: parses MCP requests, serializes responses, and
//!   forwards push notifications through a single writer task.
//! - The MykoClient holds a long-lived WebSocket connection to
//!   `MYKO_ADDRESS` (default `ws://localhost:6155`).
//! - `on_command::<NotifyChannel>` is registered before the MCP server
//!   starts; when the daemon dispatches a NotifyChannel at this client,
//!   the handler writes a `notifications/claude/channel` to stdout.
//! - Tools (set_role, roster, send_message, ...) are dispatched by
//!   `tools::CoordHandler` against the same client.

mod mcp;
mod role_instructions;
mod tools;

use anyhow::{Context, Result};
use entities::NotifyChannel;
use mcp::{ServerConfig, ToolDef};
use hyphae::Watchable;
use myko::client::{ConnectionStatus, MykoClient};
use serde_json::{Value, json};
use std::sync::Arc;

const DEFAULT_MYKO_ADDRESS: &str = "ws://localhost:6155";

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    entities::link();

    let myko_address = std::env::var("MYKO_ADDRESS")
        .unwrap_or_else(|_| DEFAULT_MYKO_ADDRESS.to_string());

    log::info!("[claude-coord-shim] connecting to {myko_address}");

    let client = Arc::new(MykoClient::new());
    let status_guard = client.connection_status().subscribe(|signal| {
        if let hyphae::Signal::Value(status) = signal {
            match &**status {
                ConnectionStatus::Connected(addr) => {
                    log::info!("[claude-coord-shim] connected to {addr}");
                }
                ConnectionStatus::Disconnected => {
                    log::warn!("[claude-coord-shim] disconnected");
                }
                _ => {}
            }
        }
    });
    client.connection_status().own(status_guard);
    client.set_address(Some(myko_address));

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

    let host = Arc::new(tools::ToolHost {
        client: Arc::clone(&client),
        pid,
        cwd: cwd.clone(),
        nickname: nickname.clone(),
    });

    let handler = Arc::new(tools::CoordHandler { host });

    let config = ServerConfig {
        name: "claude-coord-shim".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        instructions: format!(
            "Coordinate with sibling Claude sessions via the claude-coord daemon. \
             Tools: whoami, set_status, set_role, roster, send_message, inbox, \
             recent_messages. You are session '{nickname}' (cwd: {cwd}). The host \
             will deliver new-message notifications via notifications/claude/channel."
        ),
        tools: tools_def(),
    };

    let on_initialized_client = Arc::clone(&client);
    mcp::serve_stdio(config, handler, move |notifier| {
        // Subscribe to NotifyChannel commands pushed by the daemon and convert
        // each into an MCP notifications/claude/channel. The guard is leaked
        // for the lifetime of the process — we never want to unregister.
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

fn init_logging() {
    let mut b = env_logger::Builder::from_default_env();
    if std::env::var("RUST_LOG").is_err() {
        b.filter_level(log::LevelFilter::Info);
    }
    b.target(env_logger::Target::Stderr).init();
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
            description: "Return this shim's nickname, pid, and cwd.".into(),
            input_schema: empty.clone(),
        },
        ToolDef {
            name: "roster".into(),
            description: "List all live coord sessions.".into(),
            input_schema: empty,
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
    ]
}
