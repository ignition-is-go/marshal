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
    time::Instant,
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

/// Per-session notification buffer for HTTP-MCP recipients that don't
/// keep a long-lived SSE stream open. The push loop appends a frame
/// here when the recipient has no currently-open SSE channel (or the
/// write attempt fails). The next time the recipient POSTs to
/// `/myko/mcp` with `Accept: text/event-stream`, the mirror's
/// `SseConnected` handler drains this buffer into the response stream
/// before it closes, so the client receives the notifications as
/// additional SSE events alongside the JSON-RPC response.
///
/// This is the *only* path for server→client push when the client
/// doesn't keep a GET-SSE listener open (Claude Code 2.1.x's empirical
/// behaviour). For clients that DO hold an SSE stream open, live push
/// via `SseChannels` works as before and `PendingPush` stays empty.
#[derive(Clone, Default)]
pub struct PendingPush {
    inner: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

impl PendingPush {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a pre-formatted SSE frame (including the trailing
    /// `\n\n`) to the buffer for `session_id`. Called from the push
    /// loop when live delivery isn't possible.
    pub fn append(&self, session_id: &str, frame: String) {
        self.inner
            .lock()
            .expect("PendingPush mutex poisoned")
            .entry(session_id.to_string())
            .or_default()
            .push(frame);
    }

    /// Atomically remove and return everything buffered for
    /// `session_id`. Called from `SseConnected` so the drained
    /// frames can be written into the just-opened stream.
    pub fn drain(&self, session_id: &str) -> Vec<String> {
        self.inner
            .lock()
            .expect("PendingPush mutex poisoned")
            .remove(session_id)
            .unwrap_or_default()
    }

    /// Drop the buffer entry for a session entirely — called when
    /// the sweeper DELs the session so we don't leak memory on
    /// truly-gone sessions.
    pub fn forget(&self, session_id: &str) {
        self.inner
            .lock()
            .expect("PendingPush mutex poisoned")
            .remove(session_id);
    }
}

/// In-memory liveness map for HTTP-MCP sessions, keyed by `Mcp-Session-Id`.
/// The observer bumps `last_seen` on every non-initialize POST; the sweeper
/// reads it to keep sessions alive when their client (e.g. Claude Code)
/// doesn't keep an SSE channel open. Cheap to clone — wraps an
/// `Arc<Mutex<HashMap>>`.
#[derive(Clone, Default)]
pub struct LastSeen {
    inner: Arc<Mutex<HashMap<String, Instant>>>,
}

impl LastSeen {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, session_id: &str) -> Option<Instant> {
        self.inner
            .lock()
            .expect("LastSeen mutex poisoned")
            .get(session_id)
            .copied()
    }

    fn touch(&self, session_id: String) {
        self.inner
            .lock()
            .expect("LastSeen mutex poisoned")
            .insert(session_id, Instant::now());
    }

    pub fn forget(&self, session_id: &str) {
        self.inner
            .lock()
            .expect("LastSeen mutex poisoned")
            .remove(session_id);
    }
}

/// Materialises a marshal `Session` per HTTP-MCP client and tracks each
/// client's SSE push channel for later notification routing.
pub struct McpSessionMirror {
    ctx: Arc<CellServerCtx>,
    sse_channels: SseChannels,
    last_seen: LastSeen,
    pending: PendingPush,
}

impl McpSessionMirror {
    pub fn new(
        ctx: Arc<CellServerCtx>,
        sse_channels: SseChannels,
        last_seen: LastSeen,
        pending: PendingPush,
    ) -> Self {
        Self {
            ctx,
            sse_channels,
            last_seen,
            pending,
        }
    }
}

impl McpSessionObserver for McpSessionMirror {
    fn on_session_event(&self, event: McpSessionEvent) {
        match event {
            McpSessionEvent::Activity { session_id } => {
                self.last_seen.touch(session_id);
            }

            McpSessionEvent::Started {
                session_id,
                client_info,
                user_agent: _,
            } => {
                self.last_seen.touch(session_id.clone());
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
                // Drain any frames buffered while no SSE channel was open
                // into the just-opened channel. The channel's mpsc queues
                // them; `respond_sse` will pump the queue onto the wire
                // before the per-request stream closes.
                let pending = self.pending.drain(&session_id);
                for frame in pending {
                    if !channel.send_raw_frame(frame) {
                        // Channel already torn down between fire and drain —
                        // give up; nothing else can save the frames here.
                        break;
                    }
                }
                self.sse_channels.insert(session_id, channel);
            }

            McpSessionEvent::Ended { session_id } => {
                // `Ended` means "a single SSE channel for this session
                // closed" — which fires on every POST→SSE response close
                // too (Streamable HTTP: client reads response, closes
                // stream, opens a fresh POST for the next request). It
                // is NOT a "session over" signal — that's the sweeper's
                // job based on activity + open-channel rules.
                //
                // We only tear down the per-channel state here. `LastSeen`
                // is kept (Activity bumps from non-SSE POSTs are still
                // valid liveness evidence) and the Session entity is NOT
                // DEL'd. The sweeper DELs based on `client_id=None &&
                // no_open_sse && !activity_recent`.
                self.sse_channels.remove(&session_id);
            }
        }
    }
}
