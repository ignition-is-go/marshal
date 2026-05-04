use crate::messages::{Message, SessionInfo};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RosterParams {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RosterResult {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetStatusParams {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRoleParams {
    /// Free-form role name. An empty string clears the role.
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRoleResult {
    /// The role that was set (canonicalized to lowercase). Empty if cleared.
    pub role: String,
    /// Behavioral instructions for the new role. The caller should follow
    /// these going forward.
    pub instructions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OkResult {
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageParams {
    pub to: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResult {
    pub message_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxParams {
    #[serde(default = "default_true")]
    pub mark_read: bool,
}

impl Default for InboxParams {
    fn default() -> Self { Self { mark_read: true } }
}

fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxResult {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentMessagesParams {
    #[serde(default = "default_limit")]
    pub limit: u32,
}

impl Default for RecentMessagesParams {
    fn default() -> Self { Self { limit: 50 } }
}

fn default_limit() -> u32 { 50 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentMessage {
    #[serde(flatten)]
    pub message: Message,
    pub direction: Direction,
    pub to_nick: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction { Sent, Received }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentMessagesResult {
    pub messages: Vec<RecentMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoamiResult {
    pub session_id: String,
    pub nickname: String,
    pub pid: u32,
    pub cwd: std::path::PathBuf,
}

/// Method name constants — the daemon and shim must agree.
pub mod method {
    pub const ROSTER: &str = "roster";
    pub const SET_STATUS: &str = "set_status";
    pub const SET_ROLE: &str = "set_role";
    pub const SEND_MESSAGE: &str = "send_message";
    pub const INBOX: &str = "inbox";
    pub const RECENT_MESSAGES: &str = "recent_messages";
}
