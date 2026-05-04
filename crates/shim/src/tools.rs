//! Bridge between MCP tool calls and the daemon RPC client.

use crate::daemon_client::{CallError, DaemonClient};
use proto::rpc::{
    method, InboxParams, InboxResult, OkResult, RecentMessagesParams, RecentMessagesResult,
    RosterParams, RosterResult, SendMessageParams, SendMessageResult, SetStatusParams,
    WhoamiResult,
};
use std::sync::Arc;

pub struct ToolHost {
    pub client: Arc<DaemonClient>,
    pub pid: u32,
    pub cwd: std::path::PathBuf,
}

impl ToolHost {
    pub async fn whoami(&self) -> Result<WhoamiResult, CallError> {
        let (session_id, nickname) = self.client.whoami().await?;
        Ok(WhoamiResult {
            session_id,
            nickname,
            pid: self.pid,
            cwd: self.cwd.clone(),
        })
    }

    pub async fn set_status(&self, text: String) -> Result<OkResult, CallError> {
        self.client
            .call(
                method::SET_STATUS,
                serde_json::to_value(SetStatusParams { text }).unwrap(),
            )
            .await
    }

    pub async fn roster(&self) -> Result<RosterResult, CallError> {
        self.client
            .call(
                method::ROSTER,
                serde_json::to_value(RosterParams::default()).unwrap(),
            )
            .await
    }

    pub async fn send_message(
        &self,
        to: String,
        body: String,
    ) -> Result<SendMessageResult, CallError> {
        self.client
            .call(
                method::SEND_MESSAGE,
                serde_json::to_value(SendMessageParams { to, body }).unwrap(),
            )
            .await
    }

    pub async fn inbox(&self, mark_read: bool) -> Result<InboxResult, CallError> {
        self.client
            .call(
                method::INBOX,
                serde_json::to_value(InboxParams { mark_read }).unwrap(),
            )
            .await
    }

    pub async fn recent_messages(
        &self,
        limit: u32,
    ) -> Result<RecentMessagesResult, CallError> {
        self.client
            .call(
                method::RECENT_MESSAGES,
                serde_json::to_value(RecentMessagesParams { limit }).unwrap(),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::framing::{read_frame, write_frame};
    use proto::messages::ServerMsg;
    use tokio::net::UnixListener;

    async fn fake_daemon_with(
        socket: std::path::PathBuf,
        mut next: impl FnMut(&str) -> serde_json::Value + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = read_frame(&mut sock).await.unwrap();
            let welcome = ServerMsg::Welcome {
                session_id: "s-99".into(),
                nickname: "tt".into(),
            };
            write_frame(&mut sock, &serde_json::to_vec(&welcome).unwrap())
                .await
                .unwrap();
            loop {
                let frame = match read_frame(&mut sock).await {
                    Ok(f) => f,
                    Err(_) => break,
                };
                let msg: proto::messages::ClientMsg = serde_json::from_slice(&frame).unwrap();
                if let proto::messages::ClientMsg::Rpc { id, method, .. } = msg {
                    let result = next(&method);
                    let resp = ServerMsg::RpcOk { id, result };
                    write_frame(&mut sock, &serde_json::to_vec(&resp).unwrap())
                        .await
                        .unwrap();
                }
            }
        })
    }

    #[tokio::test]
    async fn whoami_returns_session_info() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let _h = fake_daemon_with(path.clone(), |_m| serde_json::json!({})).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let client = Arc::new(DaemonClient::new(path, 7, "/x".into(), None));
        let host = ToolHost {
            client,
            pid: 7,
            cwd: "/x".into(),
        };
        let w = host.whoami().await.unwrap();
        assert_eq!(w.session_id, "s-99");
        assert_eq!(w.nickname, "tt");
        assert_eq!(w.pid, 7);
    }

    #[tokio::test]
    async fn roster_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let _h = fake_daemon_with(path.clone(), |_m| serde_json::json!({"sessions": []})).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let client = Arc::new(DaemonClient::new(path, 1, "/x".into(), None));
        let host = ToolHost {
            client,
            pid: 1,
            cwd: "/x".into(),
        };
        let r = host.roster().await.unwrap();
        assert!(r.sessions.is_empty());
    }

    #[tokio::test]
    async fn send_message_calls_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let _h = fake_daemon_with(path.clone(), |m| {
            if m == method::SEND_MESSAGE {
                serde_json::json!({"message_id": 7})
            } else {
                serde_json::json!({})
            }
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let client = Arc::new(DaemonClient::new(path, 1, "/x".into(), None));
        let host = ToolHost {
            client,
            pid: 1,
            cwd: "/x".into(),
        };
        let r = host.send_message("eww".into(), "hi".into()).await.unwrap();
        assert_eq!(r.message_id, 7);
    }

    #[tokio::test]
    async fn inbox_calls_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let _h = fake_daemon_with(path.clone(), |_m| serde_json::json!({"messages": []})).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let client = Arc::new(DaemonClient::new(path, 1, "/x".into(), None));
        let host = ToolHost {
            client,
            pid: 1,
            cwd: "/x".into(),
        };
        let r = host.inbox(true).await.unwrap();
        assert!(r.messages.is_empty());
    }
}
