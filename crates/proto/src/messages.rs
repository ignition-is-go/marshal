use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        pid: u32,
        cwd: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_branch: Option<String>,
    },
    Rpc {
        id: u64,
        method: String,
        params: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        session_id: String,
        nickname: String,
    },
    RpcOk {
        id: u64,
        result: serde_json::Value,
    },
    RpcErr {
        id: u64,
        code: ErrorCode,
        message: String,
    },
    Event {
        kind: String,
        payload: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    UnknownRecipient,
    AmbiguousRecipient,
    BadRequest,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub nickname: String,
    pub pid: u32,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_task: Option<String>,
    pub connected_at: i64,
    pub last_heartbeat: i64,
    #[serde(default)]
    pub is_self: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub from_session: String,
    pub from_nick: String,
    pub body: String,
    pub sent_at: i64,
}
