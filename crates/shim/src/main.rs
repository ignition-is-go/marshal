//! claude-coord-shim — MCP server bridging the host (Claude/Cursor/etc.) to the daemon.

use anyhow::{Context, Result};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParam, CallToolResult, Content, ErrorCode, Implementation, JsonObject,
    ListToolsResult, PaginatedRequestParam, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
    ToolsCapability,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::{Error as McpError, ServiceExt};
use serde_json::{json, Value};
use shim::daemon_client::{CallError, DaemonClient};
use shim::spawn;
use shim::tools::ToolHost;
use std::sync::Arc;

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

    let client = Arc::new(DaemonClient::new(socket, pid, cwd.clone(), git_branch));
    let host = Arc::new(ToolHost {
        client,
        pid,
        cwd,
    });

    serve_rmcp(host).await
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

async fn serve_rmcp(host: Arc<ToolHost>) -> Result<()> {
    let handler = CoordServer { host };
    let running = handler
        .serve(rmcp::transport::stdio())
        .await
        .context("starting rmcp stdio server")?;
    let reason = running.waiting().await.context("rmcp service join error")?;
    tracing::info!(?reason, "rmcp server exited");
    Ok(())
}

/// Map a `daemon_client::CallError` to an MCP error.
fn mcp_err_from_call(err: CallError) -> McpError {
    match err {
        CallError::Disconnected => {
            McpError::new(ErrorCode::INTERNAL_ERROR, "daemon disconnected", None)
        }
        CallError::Rpc { code, message } => McpError::new(
            ErrorCode::INTERNAL_ERROR,
            format!("daemon error: {message}"),
            Some(json!({ "code": format!("{code:?}") })),
        ),
        CallError::Other(e) => McpError::new(
            ErrorCode::INTERNAL_ERROR,
            format!("internal: {e}"),
            None,
        ),
    }
}

fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let body = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(format!("serialize result: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}

/// Build an input schema as a JSON object: `{ "type":"object", "properties": {...}, "required": [...] }`
fn schema_object(properties: Value, required: &[&str]) -> Arc<JsonObject> {
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
    Arc::new(map)
}

#[derive(Clone)]
struct CoordServer {
    host: Arc<ToolHost>,
}

impl CoordServer {
    fn tools(&self) -> Vec<Tool> {
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
            Tool::new(
                "whoami",
                "Return this shim's session id, nickname, pid, and cwd.",
                empty.clone(),
            ),
            Tool::new(
                "set_status",
                "Set this session's free-form status text on the roster.",
                set_status,
            ),
            Tool::new(
                "roster",
                "List all live coord sessions with their last-known status.",
                empty,
            ),
            Tool::new(
                "send_message",
                "Send a message to another session by id or nickname.",
                send_message,
            ),
            Tool::new(
                "inbox",
                "Fetch unread messages addressed to this session.",
                inbox,
            ),
            Tool::new(
                "recent_messages",
                "Fetch the most recent messages addressed to this session.",
                recent_messages,
            ),
        ]
    }
}

impl ServerHandler for CoordServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::default(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability::default()),
                ..Default::default()
            },
            server_info: Implementation {
                name: "claude-coord-shim".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            instructions: Some(
                "Coordinate with sibling Claude sessions: whoami, set_status, roster, \
                 send_message, inbox, recent_messages."
                    .into(),
            ),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args = request.arguments.unwrap_or_default();
        match request.name.as_ref() {
            "whoami" => {
                let res = self.host.whoami().await.map_err(mcp_err_from_call)?;
                json_result(&res)
            }
            "set_status" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        McpError::invalid_params("set_status: missing string `text`", None)
                    })?
                    .to_string();
                let res = self.host.set_status(text).await.map_err(mcp_err_from_call)?;
                json_result(&res)
            }
            "roster" => {
                let res = self.host.roster().await.map_err(mcp_err_from_call)?;
                json_result(&res)
            }
            "send_message" => {
                let to = args
                    .get("to")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        McpError::invalid_params("send_message: missing string `to`", None)
                    })?
                    .to_string();
                let body = args
                    .get("body")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        McpError::invalid_params("send_message: missing string `body`", None)
                    })?
                    .to_string();
                let res = self
                    .host
                    .send_message(to, body)
                    .await
                    .map_err(mcp_err_from_call)?;
                json_result(&res)
            }
            "inbox" => {
                let mark_read = args
                    .get("mark_read")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let res = self
                    .host
                    .inbox(mark_read)
                    .await
                    .map_err(mcp_err_from_call)?;
                json_result(&res)
            }
            "recent_messages" => {
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|n| n.min(u32::MAX as u64) as u32)
                    .unwrap_or(50);
                let res = self
                    .host
                    .recent_messages(limit)
                    .await
                    .map_err(mcp_err_from_call)?;
                json_result(&res)
            }
            other => Err(McpError::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }

    async fn on_initialized(&self, _ctx: NotificationContext<RoleServer>) {
        tracing::info!("mcp client initialized");
    }
}
