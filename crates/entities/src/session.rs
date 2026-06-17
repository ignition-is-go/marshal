use std::sync::Arc;

use myko::{
    command::{CommandContext, CommandError},
    entities::client::ClientId,
    myko_item,
};
use serde::{Deserialize, Serialize};

/// Per-session host environment summary the shim auto-populates at startup
/// so the roster can show "which machine is this session on" without
/// every consumer running its own probe. Optional — some shims (older
/// builds, the TUI in pure-observer mode) may leave it `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInfo {
    /// `gethostname()` result. Anchors the auto-room `host:<name>`.
    pub name: String,
    /// `std::env::consts::OS` — `"linux"`, `"macos"`, `"windows"`, etc.
    pub os: String,
    /// `std::env::consts::ARCH` — `"x86_64"`, `"aarch64"`, etc.
    pub arch: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn session_old_payload_deserializes_with_default_liveness_fields() {
        // Pre-liveness sessions persisted to events.jsonl must keep
        // working — `serde(default)` on the new fields covers it.
        // This guards the daemon's persister-replay path against
        // failing to load older Session SETs.
        let payload = serde_json::json!({
            "id": "sess-001",
            "pid": 1234,
            "cwd": "/tmp",
            "connectedAt": 1_700_000_000_000_i64,
        });
        let s: Session = serde_json::from_value(payload).expect("deserialize");
        assert_eq!(s.last_activity_at, None);
        assert_eq!(s.last_tool, None);
        assert_eq!(s.last_tool_at, None);
        assert_eq!(s.operator, None);
        assert_eq!(s.host, None);
    }

    #[test]
    fn session_with_liveness_round_trips() {
        let s = Session {
            id: SessionId(Arc::from("sess-002")),
            client_id: None,
            pid: 4321,
            cwd: "/home/x".into(),
            git_branch: None,
            current_task: None,
            connected_at: 1_700_000_000_000_i64,
            last_activity_at: Some(1_700_000_005_000_i64),
            last_tool: Some("send_message".into()),
            last_tool_at: Some(1_700_000_004_500_i64),
            operator: None,
            host: None,
            project: None,
            channels_enabled: None,
        };
        let json = serde_json::to_value(&s).unwrap();
        // camelCase on the wire (matches the rest of the codebase).
        assert_eq!(json["lastActivityAt"], 1_700_000_005_000_i64);
        assert_eq!(json["lastTool"], "send_message");
        let back: Session = serde_json::from_value(json).unwrap();
        assert_eq!(back.last_activity_at, Some(1_700_000_005_000_i64));
        assert_eq!(back.last_tool.as_deref(), Some("send_message"));
        assert_eq!(back.last_tool_at, Some(1_700_000_004_500_i64));
    }

    #[test]
    fn session_with_identity_round_trips() {
        let s = Session {
            id: SessionId(Arc::from("sess-003")),
            client_id: None,
            pid: 1,
            cwd: "/repo".into(),
            git_branch: None,
            current_task: None,
            connected_at: 1_700_000_000_000_i64,
            last_activity_at: None,
            last_tool: None,
            last_tool_at: None,
            operator: Some("trevor".into()),
            project: None,
            host: Some(HostInfo {
                name: "laptop".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
            }),
            channels_enabled: None,
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["operator"], "trevor");
        assert_eq!(json["host"]["name"], "laptop");
        assert_eq!(json["host"]["os"], "linux");
        assert_eq!(json["host"]["arch"], "x86_64");
        let back: Session = serde_json::from_value(json).unwrap();
        assert_eq!(back.operator.as_deref(), Some("trevor"));
        assert_eq!(back.host.as_ref().map(|h| h.name.as_str()), Some("laptop"));
    }
}

#[myko_item]
pub struct Session {
    /// WebSocket client this session is currently bound to. Auto-populated
    /// by the server from the WS connection — clients send the SET command
    /// without providing it. There is intentionally no `belongs_to(Client)`
    /// cascade: sessions are durable roster entries that survive
    /// disconnects (and daemon restarts, via the disk persister). Whether
    /// a session's shim is currently live is determined by looking up
    /// `client_id` in the Client store; absent there ⇒ disconnected.
    #[myko_client_id]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<ClientId>,

    /// OS process id of the connecting client.
    pub pid: u32,

    /// Working directory the client launched from.
    pub cwd: String,

    /// Git branch in `cwd`, if it's a repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,

    /// Free-form short status text, settable via the `set_status` command.
    /// Surfaces on the roster so peers can see what this session is up to
    /// without needing a separate task system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[myko_setter]
    pub current_task: Option<String>,

    /// Wall-clock millis since unix epoch when the session connected.
    pub connected_at: i64,

    /// Wall-clock millis at the most recent shim-side activity (any
    /// MCP tool call). Bumped by the shim's `Activity` tracker (the
    /// same one that gates the self-update watcher's exec swap) and
    /// flushed upstream periodically — `None` until the first push,
    /// then a sliding timestamp the roster surfaces uses to compute
    /// "did this peer do anything in the last N seconds?". Stale =
    /// candidate for the stuck-session visualization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[myko_setter]
    pub last_activity_at: Option<i64>,

    /// Name of the most recent MCP tool the shim served (e.g.
    /// "send_message", "set_status"). Captures only the tool name —
    /// no arguments, no payloads — so the roster can show "this peer
    /// last called X 3s ago" without leaking message content. Pair
    /// with `last_tool_at` for staleness reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[myko_setter]
    pub last_tool: Option<String>,

    /// Wall-clock millis when `last_tool` was served. Distinct from
    /// `last_activity_at` because activity covers all shim traffic
    /// (notifications, periodic heartbeats, anything the dispatcher
    /// touches) while this is the timestamp of the last *named* MCP
    /// tool call. They typically agree, but a session that only
    /// receives notifications and never calls a tool will have a
    /// fresh `last_activity_at` and a stale `last_tool_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[myko_setter]
    pub last_tool_at: Option<i64>,

    /// Which human this session belongs to. Resolved by the shim:
    /// `$MARSHAL_OPERATOR` → `$USER` → `"anonymous"`. Anchors the
    /// auto-room `op:<operator>`, so peers can see "all of trevor's
    /// sessions" regardless of cwd. `None` for legacy / observer
    /// shims that don't set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,

    /// Host environment summary the shim populates at startup.
    /// Anchors the auto-room `host:<name>`. See `HostInfo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostInfo>,

    /// Project basename — the basename of the git repo root containing
    /// this session's `cwd`. Resolved by the shim at startup (the
    /// daemon can't run `git` against a remote shim's filesystem).
    /// Anchors the auto-room `project:<basename>`. `None` when `cwd`
    /// isn't inside a git repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,

    /// Whether this session can receive LIVE channel pushes — i.e. its
    /// claude was launched with `--dangerously-load-development-channels
    /// …marshal`. The shim resolves it from the parent process at startup;
    /// the daemon can't see it (it's a recipient-host launch detail).
    /// `None` = unknown (legacy shim that doesn't report it). `SendMessage`
    /// uses it to report `delivered_live` HONESTLY: a live WS client with
    /// channels OFF still can't render a push, so the message is queued to
    /// the inbox (delivered_live = false) rather than claimed as live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels_enabled: Option<bool>,
}

/// Resolve the session a write command is acting *as*.
///
/// Two caller-identity paths converge here:
///
/// 1. **Self-identify** (`as_session = Some`): the HTTP-MCP and hook
///    paths have no connection `client_id` — an agent calling
///    `command_SendMessage` over stock myko's HTTP transport, or the
///    daemon's `/hook/*` handlers acting on a session's behalf, name
///    the acting session explicitly. The id is the one the agent learned
///    from its SessionStart hook output (its own `session_id`).
///    Trust note: this is self-asserted — a caller can name any session
///    id. On a trusted sibling-agent mesh that's acceptable (the WS shim
///    path is no stronger), but it is the reason the daemon must not be
///    exposed off the mesh.
///
/// 2. **Connection-bound** (`as_session = None`): the WS shim path. The
///    server populated `client_id` from the live connection; we map it
///    back to the session that SET itself with that client.
///
/// Errors with an actionable message if neither resolves to a roster
/// session.
pub fn resolve_caller(
    ctx: &CommandContext,
    sessions: &[Arc<Session>],
    as_session: Option<&SessionId>,
) -> Result<Arc<Session>, CommandError> {
    if let Some(sid) = as_session {
        return sessions
            .iter()
            .find(|s| &s.id == sid)
            .cloned()
            .ok_or_else(|| {
                caller_err(
                    ctx,
                    &format!("asSession '{}' is not on the roster", sid.0.as_ref()),
                )
            });
    }

    let client_id = ctx.client_id().ok_or_else(|| {
        caller_err(
            ctx,
            "command needs either an `asSession` id (HTTP/self-identify) or a connected client (WS shim)",
        )
    })?;
    sessions
        .iter()
        .find(|s| s.client_id.as_ref() == Some(&client_id))
        .cloned()
        .ok_or_else(|| {
            caller_err(
                ctx,
                &format!(
                    "caller (client {}) has no session on the roster — re-SET your Session and retry",
                    client_id.0.as_ref(),
                ),
            )
        })
}

fn caller_err(ctx: &CommandContext, message: &str) -> CommandError {
    CommandError {
        tx: ctx.tx().to_string(),
        command_id: ctx.command_id.to_string(),
        message: message.to_string(),
    }
}
