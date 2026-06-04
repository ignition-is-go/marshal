//! Bridge `myko_server::mcp::McpSessionEvent` to marshal entities.
//!
//! Two responsibilities:
//!
//! 1. Materialise a marshal `Session` row per HTTP-MCP client on
//!    `initialize` (and DEL it on SSE close) so the connected agent
//!    appears in the roster the same way a shim-connected one does.
//!
//! 2. Hold the per-session SSE push channel so other parts of the
//!    daemon (the NotifyChannel push saga added in a follow-up) can
//!    look up where to send peer messages by session id.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use marshal_entities::{Session, SessionId};
use myko::{server::CellServerCtx, wire::MEventType};
use myko_server::mcp::{McpSessionChannel, McpSessionEvent, McpSessionObserver};

/// Lookup table for per-session SSE push channels. Cloned cheaply; the
/// inner `Mutex<HashMap>` is held only briefly for insert / remove /
/// snapshot operations.
#[derive(Clone, Default)]
pub struct SseChannels {
    inner: Arc<Mutex<HashMap<String, Arc<McpSessionChannel>>>>,
}

impl SseChannels {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, session_id: &str) -> Option<Arc<McpSessionChannel>> {
        self.inner.lock().expect("SseChannels mutex poisoned").get(session_id).cloned()
    }

    fn insert(&self, session_id: String, channel: Arc<McpSessionChannel>) {
        self.inner
            .lock()
            .expect("SseChannels mutex poisoned")
            .insert(session_id, channel);
    }

    fn remove(&self, session_id: &str) {
        self.inner
            .lock()
            .expect("SseChannels mutex poisoned")
            .remove(session_id);
    }
}

/// Materialises a marshal `Session` per HTTP-MCP client and tracks each
/// client's SSE push channel for later notification routing.
pub struct McpSessionMirror {
    ctx: Arc<CellServerCtx>,
    sse_channels: SseChannels,
}

impl McpSessionMirror {
    pub fn new(ctx: Arc<CellServerCtx>, sse_channels: SseChannels) -> Self {
        Self { ctx, sse_channels }
    }
}

impl McpSessionObserver for McpSessionMirror {
    fn on_session_event(&self, event: McpSessionEvent) {
        match event {
            McpSessionEvent::Started {
                session_id,
                client_info,
                user_agent: _,
            } => {
                let short = session_id.chars().take(8).collect::<String>();
                let client_name = client_info
                    .as_ref()
                    .map(|c| c.name.as_str())
                    .unwrap_or("mcp-http");
                let nickname = format!("{client_name}@{short}");

                let session = Session {
                    id: SessionId(Arc::from(session_id.as_str())),
                    client_id: None,
                    nickname,
                    pid: 0,
                    cwd: String::new(),
                    git_branch: None,
                    current_task: None,
                    connected_at: Utc::now().timestamp_millis(),
                    last_activity_at: None,
                    last_tool: None,
                    last_tool_at: None,
                    operator: None,
                    host: None,
                    project: None,
                };

                let ev = myko::wire::MEvent::from_item(
                    &session,
                    MEventType::SET,
                    &uuid::Uuid::new_v4().to_string(),
                );
                if let Err(e) = self.ctx.apply_event_batch(vec![ev]) {
                    log::warn!("[mcp-observer] failed to SET Session on initialize: {e}");
                }
            }

            McpSessionEvent::SseConnected { session_id, channel } => {
                self.sse_channels.insert(session_id, channel);
            }

            McpSessionEvent::Ended { session_id } => {
                self.sse_channels.remove(&session_id);

                let stub = Session {
                    id: SessionId(Arc::from(session_id.as_str())),
                    client_id: None,
                    nickname: String::new(),
                    pid: 0,
                    cwd: String::new(),
                    git_branch: None,
                    current_task: None,
                    connected_at: 0,
                    last_activity_at: None,
                    last_tool: None,
                    last_tool_at: None,
                    operator: None,
                    host: None,
                    project: None,
                };
                let ev = myko::wire::MEvent::from_item(
                    &stub,
                    MEventType::DEL,
                    &uuid::Uuid::new_v4().to_string(),
                );
                if let Err(e) = self.ctx.apply_event_batch(vec![ev]) {
                    log::warn!("[mcp-observer] failed to DEL Session on close: {e}");
                }
            }
        }
    }
}
