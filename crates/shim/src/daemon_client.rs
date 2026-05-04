//! Minimal RPC client over the daemon's unix socket.

use anyhow::{anyhow, Result};
use proto::framing::{read_frame, write_frame};
use proto::messages::{ClientMsg, ErrorCode, ServerMsg};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

pub struct DaemonClient {
    socket_path: PathBuf,
    conn: Mutex<Option<Conn>>,
    next_id: AtomicU64,
    pid: u32,
    cwd: PathBuf,
    git_branch: Option<String>,
}

struct Conn {
    sock: UnixStream,
    pub session_id: String,
    pub nickname: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("daemon disconnected")]
    Disconnected,
    #[error("rpc error [{code:?}]: {message}")]
    Rpc { code: ErrorCode, message: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl DaemonClient {
    pub fn new(socket_path: PathBuf, pid: u32, cwd: PathBuf, git_branch: Option<String>) -> Self {
        Self {
            socket_path,
            conn: Mutex::new(None),
            next_id: AtomicU64::new(1),
            pid,
            cwd,
            git_branch,
        }
    }

    pub async fn ensure_connected(&self) -> Result<(String, String), CallError> {
        let mut g = self.conn.lock().await;
        if let Some(c) = g.as_ref() {
            return Ok((c.session_id.clone(), c.nickname.clone()));
        }
        let mut sock = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|_| CallError::Disconnected)?;
        let hello = ClientMsg::Hello {
            pid: self.pid,
            cwd: self.cwd.clone(),
            git_branch: self.git_branch.clone(),
        };
        write_frame(
            &mut sock,
            serde_json::to_vec(&hello).map_err(|e| anyhow!(e))?.as_slice(),
        )
        .await
        .map_err(|e| anyhow!(e))?;
        let frame = read_frame(&mut sock).await.map_err(|e| anyhow!(e))?;
        let resp: ServerMsg = serde_json::from_slice(&frame).map_err(|e| anyhow!(e))?;
        let (session_id, nickname) = match resp {
            ServerMsg::Welcome {
                session_id,
                nickname,
            } => (session_id, nickname),
            other => return Err(anyhow!("expected welcome, got {other:?}").into()),
        };
        *g = Some(Conn {
            sock,
            session_id: session_id.clone(),
            nickname: nickname.clone(),
        });
        Ok((session_id, nickname))
    }

    /// Drop any cached connection so the next call reconnects.
    pub async fn force_reconnect(&self) {
        *self.conn.lock().await = None;
    }

    pub async fn call<R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<R, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = ClientMsg::Rpc {
            id,
            method: method.into(),
            params,
        };

        // Best-effort send with one reconnect attempt on disconnected.
        for attempt in 0..2 {
            self.ensure_connected().await?;
            let mut g = self.conn.lock().await;
            let conn = g.as_mut().ok_or(CallError::Disconnected)?;
            let buf = serde_json::to_vec(&req).map_err(|e| anyhow!(e))?;
            if write_frame(&mut conn.sock, &buf).await.is_err() {
                drop(g);
                self.force_reconnect().await;
                if attempt == 0 {
                    continue;
                }
                return Err(CallError::Disconnected);
            }
            let frame = match read_frame(&mut conn.sock).await {
                Ok(f) => f,
                Err(_) => {
                    drop(g);
                    self.force_reconnect().await;
                    if attempt == 0 {
                        continue;
                    }
                    return Err(CallError::Disconnected);
                }
            };
            let resp: ServerMsg = serde_json::from_slice(&frame).map_err(|e| anyhow!(e))?;
            return match resp {
                ServerMsg::RpcOk { id: rid, result } if rid == id => {
                    Ok(serde_json::from_value(result).map_err(|e| anyhow!(e))?)
                }
                ServerMsg::RpcOk { id: rid, .. } => {
                    Err(anyhow!("response id mismatch: req {id}, got {rid}").into())
                }
                ServerMsg::RpcErr {
                    id: rid,
                    code,
                    message,
                } if rid == id => Err(CallError::Rpc { code, message }),
                ServerMsg::RpcErr { id: rid, .. } => {
                    Err(anyhow!("error id mismatch: req {id}, got {rid}").into())
                }
                ServerMsg::Welcome { .. } | ServerMsg::Event { .. } => {
                    Err(anyhow!("unexpected message during call").into())
                }
            };
        }
        Err(CallError::Disconnected)
    }

    pub async fn whoami(&self) -> Result<(String, String), CallError> {
        self.ensure_connected().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::framing::{read_frame, write_frame};
    use std::sync::Arc;
    use tokio::net::UnixListener;

    async fn fake_daemon(socket: std::path::PathBuf) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Read hello, send welcome.
            let _ = read_frame(&mut sock).await.unwrap();
            let welcome = ServerMsg::Welcome {
                session_id: "s-test".into(),
                nickname: "shim-test".into(),
            };
            write_frame(&mut sock, &serde_json::to_vec(&welcome).unwrap())
                .await
                .unwrap();
            // Echo any RPC with an Ok of {echo: method}
            loop {
                let frame = match read_frame(&mut sock).await {
                    Ok(f) => f,
                    Err(_) => break,
                };
                let msg: ClientMsg = serde_json::from_slice(&frame).unwrap();
                if let ClientMsg::Rpc { id, method, .. } = msg {
                    let resp = ServerMsg::RpcOk {
                        id,
                        result: serde_json::json!({"echo": method}),
                    };
                    write_frame(&mut sock, &serde_json::to_vec(&resp).unwrap())
                        .await
                        .unwrap();
                }
            }
        })
    }

    #[tokio::test]
    async fn call_succeeds_against_fake_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let _ = fake_daemon(path.clone()).await;
        // Give the listener a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let client = Arc::new(DaemonClient::new(
            path,
            42,
            std::path::PathBuf::from("/x/eww"),
            None,
        ));
        let (sid, nick) = client.whoami().await.unwrap();
        assert_eq!(sid, "s-test");
        assert_eq!(nick, "shim-test");

        let v: serde_json::Value = client.call("roster", serde_json::json!({})).await.unwrap();
        assert_eq!(v, serde_json::json!({"echo": "roster"}));
    }

    #[tokio::test]
    async fn disconnect_propagates_when_daemon_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist");
        let client = DaemonClient::new(path, 1, std::path::PathBuf::from("/x"), None);
        let err = client
            .call::<serde_json::Value>("roster", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Disconnected));
    }
}
