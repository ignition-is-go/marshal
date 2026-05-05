use myko::{entities::client::ClientId, myko_item};

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
    pub nickname: String,

    /// OS process id of the connecting client.
    pub pid: u32,

    /// Working directory the client launched from.
    pub cwd: String,

    /// Git branch in `cwd`, if it's a repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,

    /// Free-form short status text, settable via the `set_status` command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[myko_setter]
    pub current_task: Option<String>,

    /// Coordination role (e.g. "worker", "task_distributor", "communicator").
    /// Empty/None means no role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[myko_setter]
    pub role: Option<String>,

    /// Wall-clock millis since unix epoch when the session connected.
    pub connected_at: i64,
}
