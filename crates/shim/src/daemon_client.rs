//! RPC client over the daemon's unix socket with async event multiplexing.
//!
//! A long-lived reader task demuxes incoming frames:
//! - `ServerMsg::RpcOk`/`RpcErr` are routed to the `pending` map by request id.
//! - `ServerMsg::Event` is forwarded to the events channel.
//!
//! `call()` is request/response over a oneshot. The reader task fires the oneshot
//! when the matching reply arrives. On disconnect the reader drains all pending
//! oneshots with a `Disconnected` reply.

use anyhow::{anyhow, Result};
use proto::framing::{read_frame, write_frame};
use proto::messages::{ClientMsg, ErrorCode, ServerMsg};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex as TokioMutex};

/// Push event delivered from the daemon (e.g. a new_message arrived for this session).
#[derive(Debug, Clone)]
pub struct EventMsg {
    pub kind: String,
    pub payload: serde_json::Value,
}

#[derive(Debug)]
enum RpcReply {
    Ok(serde_json::Value),
    Err(ErrorCode, String),
    Disconnected,
}

pub struct DaemonClient {
    socket_path: PathBuf,
    inner: TokioMutex<Option<ConnInner>>,
    next_id: AtomicU64,
    pid: u32,
    cwd: PathBuf,
    git_branch: Option<String>,
    events_tx: mpsc::UnboundedSender<EventMsg>,
    events_rx: TokioMutex<Option<mpsc::UnboundedReceiver<EventMsg>>>,
}

struct ConnInner {
    write_half: tokio::net::unix::OwnedWriteHalf,
    pending: Arc<TokioMutex<HashMap<u64, oneshot::Sender<RpcReply>>>>,
    session_id: String,
    nickname: String,
    /// Reader task handle. When `inner` is dropped/replaced, the JoinHandle is
    /// dropped; the task itself naturally exits when the read half EOFs.
    _reader_task: tokio::task::JoinHandle<()>,
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
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        Self {
            socket_path,
            inner: TokioMutex::new(None),
            next_id: AtomicU64::new(1),
            pid,
            cwd,
            git_branch,
            events_tx,
            events_rx: TokioMutex::new(Some(events_rx)),
        }
    }

    /// Take the events receiver. Returns `Some` exactly once per `DaemonClient`
    /// instance; subsequent calls return `None`. The consumer should spawn a
    /// task that reads from the receiver and forwards events as it sees fit
    /// (e.g. emit MCP `notifications/claude/channel`).
    pub async fn take_events_rx(&self) -> Option<mpsc::UnboundedReceiver<EventMsg>> {
        self.events_rx.lock().await.take()
    }

    pub async fn ensure_connected(&self) -> Result<(String, String), CallError> {
        let mut g = self.inner.lock().await;
        if let Some(c) = g.as_ref() {
            return Ok((c.session_id.clone(), c.nickname.clone()));
        }

        let sock = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|_| CallError::Disconnected)?;
        let (read_half, mut write_half) = sock.into_split();

        // Hello.
        let hello = ClientMsg::Hello {
            pid: self.pid,
            cwd: self.cwd.clone(),
            git_branch: self.git_branch.clone(),
        };
        let buf = serde_json::to_vec(&hello).map_err(|e| anyhow!(e))?;
        write_frame(&mut write_half, &buf).await.map_err(|e| anyhow!(e))?;

        // Welcome (consumed before the reader task starts).
        let mut read_half = read_half;
        let frame = read_frame(&mut read_half).await.map_err(|e| anyhow!(e))?;
        let resp: ServerMsg = serde_json::from_slice(&frame).map_err(|e| anyhow!(e))?;
        let (session_id, nickname) = match resp {
            ServerMsg::Welcome { session_id, nickname } => (session_id, nickname),
            other => return Err(anyhow!("expected welcome, got {other:?}").into()),
        };

        let pending: Arc<TokioMutex<HashMap<u64, oneshot::Sender<RpcReply>>>> =
            Arc::new(TokioMutex::new(HashMap::new()));
        let pending_for_reader = Arc::clone(&pending);
        let events_tx = self.events_tx.clone();

        let reader_task = tokio::spawn(async move {
            loop {
                let frame = match read_frame(&mut read_half).await {
                    Ok(f) => f,
                    Err(_) => break,
                };
                let msg: ServerMsg = match serde_json::from_slice(&frame) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                match msg {
                    ServerMsg::RpcOk { id, result } => {
                        if let Some(tx) = pending_for_reader.lock().await.remove(&id) {
                            let _ = tx.send(RpcReply::Ok(result));
                        }
                    }
                    ServerMsg::RpcErr { id, code, message } => {
                        if let Some(tx) = pending_for_reader.lock().await.remove(&id) {
                            let _ = tx.send(RpcReply::Err(code, message));
                        }
                    }
                    ServerMsg::Event { kind, payload } => {
                        let _ = events_tx.send(EventMsg { kind, payload });
                    }
                    ServerMsg::Welcome { .. } => {
                        // Daemon should not re-send welcome; ignore defensively.
                    }
                }
            }
            // Read half closed: drain pending with Disconnected.
            let mut p = pending_for_reader.lock().await;
            for (_, tx) in p.drain() {
                let _ = tx.send(RpcReply::Disconnected);
            }
        });

        *g = Some(ConnInner {
            write_half,
            pending,
            session_id: session_id.clone(),
            nickname: nickname.clone(),
            _reader_task: reader_task,
        });
        Ok((session_id, nickname))
    }

    /// Drop any cached connection so the next call reconnects.
    pub async fn force_reconnect(&self) {
        *self.inner.lock().await = None;
    }

    pub async fn call<R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<R, CallError> {
        self.ensure_connected().await?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel::<RpcReply>();

        // Register the oneshot under the request id and write the frame.
        {
            let mut g = self.inner.lock().await;
            let inner = g.as_mut().ok_or(CallError::Disconnected)?;
            inner.pending.lock().await.insert(id, reply_tx);

            let req = ClientMsg::Rpc {
                id,
                method: method.into(),
                params,
            };
            let buf = serde_json::to_vec(&req).map_err(|e| anyhow!(e))?;
            if write_frame(&mut inner.write_half, &buf).await.is_err() {
                inner.pending.lock().await.remove(&id);
                *g = None;
                return Err(CallError::Disconnected);
            }
        }

        match reply_rx.await {
            Ok(RpcReply::Ok(value)) => {
                Ok(serde_json::from_value(value).map_err(|e| anyhow!(e))?)
            }
            Ok(RpcReply::Err(code, message)) => Err(CallError::Rpc { code, message }),
            Ok(RpcReply::Disconnected) => {
                *self.inner.lock().await = None;
                Err(CallError::Disconnected)
            }
            Err(_) => {
                // Sender dropped without a value (reader task panicked or pending entry was evicted).
                *self.inner.lock().await = None;
                Err(CallError::Disconnected)
            }
        }
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
        let _h = fake_daemon(path.clone()).await;
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

    #[tokio::test]
    async fn events_are_forwarded_to_subscriber() {
        // Fake daemon that pushes an Event after sending Welcome.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let listener = UnixListener::bind(&path).unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = read_frame(&mut sock).await.unwrap();
            let welcome = ServerMsg::Welcome {
                session_id: "s-evt".into(),
                nickname: "tt".into(),
            };
            write_frame(&mut sock, &serde_json::to_vec(&welcome).unwrap())
                .await
                .unwrap();
            // Push an event.
            let event = ServerMsg::Event {
                kind: "new_message".into(),
                payload: serde_json::json!({"body": "hi"}),
            };
            write_frame(&mut sock, &serde_json::to_vec(&event).unwrap())
                .await
                .unwrap();
            // Hold the connection open briefly.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let client = DaemonClient::new(path, 1, std::path::PathBuf::from("/x"), None);
        let mut events_rx = client.take_events_rx().await.unwrap();
        let _ = client.whoami().await.unwrap();

        let event = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            events_rx.recv(),
        )
        .await
        .expect("timed out waiting for event")
        .expect("event channel closed");
        assert_eq!(event.kind, "new_message");
        assert_eq!(event.payload["body"], "hi");
    }
}
