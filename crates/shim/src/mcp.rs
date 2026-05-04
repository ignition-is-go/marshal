//! Minimal MCP (Model Context Protocol) server over stdio.
//!
//! Implements just enough of MCP for the claude-coord-shim's needs:
//! - line-delimited JSON-RPC 2.0 framing on stdin/stdout
//! - `initialize` / `tools/list` / `tools/call` / `ping` request handlers
//! - `notifications/initialized` listener
//! - server-side notifications, including the experimental
//!   `notifications/claude/channel` extension (which `rmcp 0.2` cannot send).
//!
//! Outbound writes go through a single writer task so notifications can't
//! interleave with response bytes on stdout.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

// =============================================================================
// Public API: types you describe your server with
// =============================================================================

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub name: String,
    pub version: String,
    pub instructions: String,
    pub tools: Vec<ToolDef>,
}

/// Result of a tool call.
#[derive(Debug)]
pub enum ToolOutcome {
    /// One or more text content blocks. Most callers want this.
    Text(String),
    /// Pretty-printed JSON in a single text block.
    Json(Value),
}

/// Error from a tool handler. `code` follows JSON-RPC error codes.
#[derive(Debug)]
pub struct ToolError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl ToolError {
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: INVALID_PARAMS,
            message: msg.into(),
            data: None,
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: INTERNAL_ERROR,
            message: msg.into(),
            data: None,
        }
    }
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

/// Handle for sending push notifications to the connected MCP client.
/// Cheap to clone; clones share the same outbound channel.
#[derive(Clone)]
pub struct Notifier {
    out_tx: mpsc::UnboundedSender<OutboundMessage>,
}

impl Notifier {
    /// Send a `notifications/claude/channel` event. `meta` becomes the wire
    /// `meta` field; `content` becomes the `content` field.
    ///
    /// Claude Code validates `params.meta` as `Record<string, string>` (Zod
    /// schema in 2.1.x), so any non-string values silently fail validation
    /// and the notification is dropped. We coerce nested values to JSON
    /// strings here so callers can pass arbitrary objects without thinking
    /// about the wire constraint.
    pub fn channel(&self, content: impl Into<String>, meta: Value) {
        let coerced = match meta {
            Value::Object(map) => {
                let stringified: serde_json::Map<String, Value> = map
                    .into_iter()
                    .map(|(k, v)| {
                        let s = match v {
                            Value::String(s) => s,
                            other => other.to_string(),
                        };
                        (k, Value::String(s))
                    })
                    .collect();
                Value::Object(stringified)
            }
            _ => Value::Object(serde_json::Map::new()),
        };
        let params = serde_json::json!({
            "content": content.into(),
            "meta": coerced,
        });
        self.send_raw("notifications/claude/channel", params);
    }

    /// Send a `notifications/message` (logging) event.
    pub fn log(&self, level: &str, data: Value) {
        let params = serde_json::json!({
            "level": level,
            "logger": "claude-coord",
            "data": data,
        });
        self.send_raw("notifications/message", params);
    }

    /// Send an arbitrary JSON-RPC notification with the given method/params.
    pub fn send_raw(&self, method: &str, params: Value) {
        let _ = self.out_tx.send(OutboundMessage::Notification {
            method: method.to_string(),
            params,
        });
    }
}

// =============================================================================
// Wire types
// =============================================================================

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    /// `id` distinguishes requests (Some) from notifications (None).
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcSuccess<'a> {
    jsonrpc: &'static str,
    id: &'a Value,
    result: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorReply<'a> {
    jsonrpc: &'static str,
    id: &'a Value,
    error: JsonRpcErrorBody,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorBody {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcNotification {
    jsonrpc: &'static str,
    method: String,
    params: Value,
}

#[derive(Debug)]
enum OutboundMessage {
    Reply { id: Value, result: Value },
    Error { id: Value, body: JsonRpcErrorBody },
    Notification { method: String, params: Value },
}

// =============================================================================
// Tool handler trait, with explicit boxed future for object-safety
// =============================================================================

pub type ToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolOutcome, ToolError>> + Send + 'a>>;

pub trait ToolHandler: Send + Sync + 'static {
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        notifier: &'a Notifier,
    ) -> ToolFuture<'a>;
}

// =============================================================================
// Server
// =============================================================================

/// Run the MCP server on stdin/stdout. `on_initialized` is invoked once after
/// the client sends `notifications/initialized`; use the Notifier to send any
/// startup notifications and to spawn long-lived event forwarders.
pub async fn serve_stdio<H, F>(
    config: ServerConfig,
    handler: Arc<H>,
    on_initialized: F,
) -> Result<()>
where
    H: ToolHandler,
    F: FnOnce(Notifier) + Send + 'static,
{
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    serve(config, handler, on_initialized, stdin, stdout).await
}

/// Like `serve_stdio` but with explicit transport handles (used by tests).
pub async fn serve<H, F, R, W>(
    config: ServerConfig,
    handler: Arc<H>,
    on_initialized: F,
    reader: R,
    writer: W,
) -> Result<()>
where
    H: ToolHandler,
    F: FnOnce(Notifier) + Send + 'static,
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<OutboundMessage>();
    let notifier = Notifier { out_tx };
    let writer = Arc::new(Mutex::new(writer));

    // Writer task: serialize outbound messages and write them as
    // newline-delimited JSON on stdout. A single writer guarantees no
    // interleaving between responses and notifications.
    let writer_task = {
        let writer = Arc::clone(&writer);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let line = match &msg {
                    OutboundMessage::Reply { id, result } => {
                        let r = JsonRpcSuccess {
                            jsonrpc: "2.0",
                            id,
                            result: result.clone(),
                        };
                        match serde_json::to_string(&r) {
                            Ok(s) => s,
                            Err(e) => {
                                log::warn!("serialize reply: {e}");
                                continue;
                            }
                        }
                    }
                    OutboundMessage::Error { id, body } => {
                        let r = JsonRpcErrorReply {
                            jsonrpc: "2.0",
                            id,
                            error: JsonRpcErrorBody {
                                code: body.code,
                                message: body.message.clone(),
                                data: body.data.clone(),
                            },
                        };
                        match serde_json::to_string(&r) {
                            Ok(s) => s,
                            Err(e) => {
                                log::warn!("serialize error: {e}");
                                continue;
                            }
                        }
                    }
                    OutboundMessage::Notification { method, params } => {
                        let n = JsonRpcNotification {
                            jsonrpc: "2.0",
                            method: method.clone(),
                            params: params.clone(),
                        };
                        match serde_json::to_string(&n) {
                            Ok(s) => s,
                            Err(e) => {
                                log::warn!("serialize notification: {e}");
                                continue;
                            }
                        }
                    }
                };
                let mut w = writer.lock().await;
                if w.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if w.write_all(b"\n").await.is_err() {
                    break;
                }
                if w.flush().await.is_err() {
                    break;
                }
            }
        })
    };

    // Reader loop: parse line-delimited JSON-RPC requests/notifications and
    // dispatch them. Spawns each `tools/call` so a slow tool can't block other
    // requests.
    let mut reader = BufReader::new(reader);
    let mut on_initialized = Some(on_initialized);
    let mut line = String::new();
    loop {
        line.clear();
        let n = match reader.read_line(&mut line).await {
            Ok(n) => n,
            Err(e) => {
                log::warn!("stdin read error: {e}");
                break;
            }
        };
        if n == 0 {
            break; // EOF: client disconnected
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("malformed json-rpc: {e} | line: {trimmed}");
                // Per JSON-RPC, we'd reply with parse_error if we had an id.
                continue;
            }
        };

        match req.id {
            Some(id) => dispatch_request(
                req.method,
                req.params,
                id,
                &config,
                Arc::clone(&handler),
                notifier.clone(),
            ),
            None => {
                if req.method == "notifications/initialized" {
                    if let Some(cb) = on_initialized.take() {
                        cb(notifier.clone());
                    }
                } else {
                    log::debug!("ignoring notification: {}", req.method);
                }
            }
        }
    }

    drop(notifier); // close the channel so the writer task exits
    let _ = writer_task.await;
    Ok(())
}

fn dispatch_request<H>(
    method: String,
    params: Value,
    id: Value,
    config: &ServerConfig,
    handler: Arc<H>,
    notifier: Notifier,
) where
    H: ToolHandler,
{
    match method.as_str() {
        "initialize" => {
            let result = serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {},
                    "logging": {},
                    "experimental": {
                        "claude/channel": {}
                    }
                },
                "serverInfo": {
                    "name": config.name,
                    "version": config.version,
                },
                "instructions": config.instructions,
            });
            let _ = notifier
                .out_tx
                .send(OutboundMessage::Reply { id, result });
        }
        "ping" => {
            let _ = notifier.out_tx.send(OutboundMessage::Reply {
                id,
                result: serde_json::json!({}),
            });
        }
        "tools/list" => {
            let result = serde_json::json!({ "tools": &config.tools });
            let _ = notifier
                .out_tx
                .send(OutboundMessage::Reply { id, result });
        }
        "tools/call" => {
            // Spawn so a slow tool doesn't block the read loop.
            tokio::spawn(async move {
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);

                let result = handler.call_tool(&name, &args, &notifier).await;
                let msg = match result {
                    Ok(outcome) => OutboundMessage::Reply {
                        id,
                        result: tool_outcome_to_result(outcome),
                    },
                    Err(e) => OutboundMessage::Error {
                        id,
                        body: JsonRpcErrorBody {
                            code: e.code,
                            message: e.message,
                            data: e.data,
                        },
                    },
                };
                let _ = notifier.out_tx.send(msg);
            });
        }
        other => {
            let body = JsonRpcErrorBody {
                code: METHOD_NOT_FOUND,
                message: format!("method '{other}' not implemented"),
                data: None,
            };
            let _ = notifier.out_tx.send(OutboundMessage::Error { id, body });
        }
    }
}

fn tool_outcome_to_result(outcome: ToolOutcome) -> Value {
    let text = match outcome {
        ToolOutcome::Text(s) => s,
        ToolOutcome::Json(v) => {
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
        }
    };
    serde_json::json!({
        "content": [
            { "type": "text", "text": text }
        ],
        "isError": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    struct EchoHandler;
    impl ToolHandler for EchoHandler {
        fn call_tool<'a>(
            &'a self,
            name: &'a str,
            args: &'a Value,
            _notifier: &'a Notifier,
        ) -> ToolFuture<'a> {
            Box::pin(async move {
                Ok(ToolOutcome::Json(serde_json::json!({
                    "tool": name,
                    "args": args,
                })))
            })
        }
    }

    fn make_config() -> ServerConfig {
        ServerConfig {
            name: "test".into(),
            version: "0.0.0".into(),
            instructions: "test instructions".into(),
            tools: vec![ToolDef {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
        }
    }

    async fn read_line(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> String {
        let mut line = String::new();
        let mut buf = BufReader::new(reader);
        buf.read_line(&mut line).await.unwrap();
        line
    }

    #[tokio::test]
    async fn initialize_returns_capabilities() {
        let (client_w, server_r) = duplex(64 * 1024);
        let (server_w, client_r) = duplex(64 * 1024);

        let server = tokio::spawn(serve(
            make_config(),
            Arc::new(EchoHandler),
            |_| {},
            server_r,
            server_w,
        ));

        let mut client_w = client_w;
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        });
        client_w
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();

        let mut client_r = client_r;
        let line = read_line(&mut client_r).await;
        let resp: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(resp["result"]["serverInfo"]["name"], "test");
        assert!(resp["result"]["capabilities"]["experimental"]["claude/channel"].is_object());

        drop(client_w); // close stdin so server exits
        let _ = server.await;
    }

    #[tokio::test]
    async fn tools_list_returns_definitions() {
        let (client_w, server_r) = duplex(64 * 1024);
        let (server_w, client_r) = duplex(64 * 1024);

        let server = tokio::spawn(serve(
            make_config(),
            Arc::new(EchoHandler),
            |_| {},
            server_r,
            server_w,
        ));

        let mut client_w = client_w;
        client_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();

        let mut client_r = client_r;
        let line = read_line(&mut client_r).await;
        let resp: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(resp["id"], 2);
        assert_eq!(resp["result"]["tools"][0]["name"], "echo");

        drop(client_w);
        let _ = server.await;
    }

    #[tokio::test]
    async fn tools_call_dispatches_and_serializes() {
        let (client_w, server_r) = duplex(64 * 1024);
        let (server_w, client_r) = duplex(64 * 1024);

        let server = tokio::spawn(serve(
            make_config(),
            Arc::new(EchoHandler),
            |_| {},
            server_r,
            server_w,
        ));

        let mut client_w = client_w;
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "echo", "arguments": {"hello": "world"} }
        });
        client_w
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();

        let mut client_r = client_r;
        let line = read_line(&mut client_r).await;
        let resp: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(resp["id"], 3);
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let inner: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(inner["tool"], "echo");
        assert_eq!(inner["args"]["hello"], "world");

        drop(client_w);
        let _ = server.await;
    }

    #[tokio::test]
    async fn on_initialized_callback_can_send_channel_notification() {
        let (client_w, server_r) = duplex(64 * 1024);
        let (server_w, client_r) = duplex(64 * 1024);

        let server = tokio::spawn(serve(
            make_config(),
            Arc::new(EchoHandler),
            |notifier| {
                notifier.channel("hello", serde_json::json!({"source": "test"}));
            },
            server_r,
            server_w,
        ));

        let mut client_w = client_w;
        // Initialize handshake then send notifications/initialized.
        client_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
            .await
            .unwrap();
        let mut client_r = client_r;
        let _ = read_line(&mut client_r).await; // initialize reply

        client_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();

        let line = read_line(&mut client_r).await;
        let n: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(n["method"], "notifications/claude/channel");
        assert_eq!(n["params"]["content"], "hello");
        assert_eq!(n["params"]["meta"]["source"], "test");

        drop(client_w);
        let _ = server.await;
    }
}
