use myko::{entities::client::ClientId, myko_item};

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
            "nickname": "old",
            "pid": 1234,
            "cwd": "/tmp",
            "connectedAt": 1_700_000_000_000_i64,
        });
        let s: Session = serde_json::from_value(payload).expect("deserialize");
        assert_eq!(s.last_activity_at, None);
        assert_eq!(s.last_tool, None);
        assert_eq!(s.last_tool_at, None);
    }

    #[test]
    fn session_with_liveness_round_trips() {
        let s = Session {
            id: SessionId(Arc::from("sess-002")),
            client_id: None,
            nickname: "live".into(),
            pid: 4321,
            cwd: "/home/x".into(),
            git_branch: None,
            current_task: None,
            connected_at: 1_700_000_000_000_i64,
            last_activity_at: Some(1_700_000_005_000_i64),
            last_tool: Some("send_message".into()),
            last_tool_at: Some(1_700_000_004_500_i64),
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

    /// Free-form display name. Defaults to the cwd basename for shims;
    /// the TUI sets it to "tui" so it isn't confused with a Claude session.
    /// The daemon's `DedupeNicknameSaga` rewrites colliding names by
    /// appending `-{N}` so peers always have a unique addressable handle.
    #[myko_setter]
    pub nickname: String,

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
}
