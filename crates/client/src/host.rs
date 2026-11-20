//! The outbound connection from a Marshal host to the server.

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{
        Message, client::IntoClientRequest, handshake::client::Request, protocol::WebSocketConfig,
    },
};

pub const HOST_PROTOCOL_VERSION: u16 = 1;
pub const HOST_MESSAGE_LIMIT: usize = 64 * 1024;
pub const HOST_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

pub fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(HOST_MESSAGE_LIMIT))
        .max_frame_size(Some(HOST_MESSAGE_LIMIT))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Claude,
    Codex,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostRegistration {
    pub protocol: u16,
    /// An empty list connects the host for session delivery only.
    pub launch_harnesses: Vec<Harness>,
}

/// A stable launch ID is reused when a dispatch is retried.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "operation", deny_unknown_fields)]
pub enum ExecutionRequest {
    Start {
        launch_id: String,
        harness: Harness,
        workspace: String,
    },
    Inspect {
        launch_id: String,
    },
    Stop {
        launch_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status", deny_unknown_fields)]
pub enum ExecutionResult {
    Running {
        handle: String,
    },
    Attached {
        handle: String,
        session_id: String,
    },
    Accepted {
        handle: String,
        session_id: String,
        task_id: String,
    },
    Stopped,
    Unknown,
    Managed {
        handle: String,
        host: String,
        cwd: String,
    },
    /// An authoritative inspection found a terminal failure with no session to join.
    Failed {
        reason: String,
    },
    Rejected {
        reason: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub enum HostMessage {
    Register(HostRegistration),
    Heartbeat,
    ExecutionResult {
        request_id: String,
        result: ExecutionResult,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub enum ServerMessage {
    Registered {
        host_id: String,
    },
    Heartbeat,
    Execute {
        request_id: String,
        request: ExecutionRequest,
    },
}

pub struct HostConnection {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    pub host_id: String,
}

impl HostConnection {
    pub async fn connect(
        url: &str,
        host_id: &str,
        token: &str,
        launch_harnesses: Vec<Harness>,
    ) -> anyhow::Result<Self> {
        let mut request: Request = url.into_client_request()?;
        let uri = request.uri();
        anyhow::ensure!(
            uri.scheme_str() == Some("wss")
                || (uri.scheme_str() == Some("ws")
                    && matches!(uri.host(), Some("127.0.0.1" | "[::1]" | "localhost"))),
            "remote host connections require wss"
        );
        request
            .headers_mut()
            .insert("x-marshal-host", host_id.parse()?);
        let mut authorization: tokio_tungstenite::tungstenite::http::HeaderValue =
            format!("Bearer {token}").parse()?;
        authorization.set_sensitive(true);
        request.headers_mut().insert("authorization", authorization);
        let (mut socket, _) =
            connect_async_with_config(request, Some(websocket_config()), false).await?;
        let register = HostMessage::Register(HostRegistration {
            protocol: HOST_PROTOCOL_VERSION,
            launch_harnesses,
        });
        socket
            .send(Message::text(serde_json::to_string(&register)?))
            .await?;
        let ServerMessage::Registered { host_id: confirmed } =
            read_server_message(&mut socket).await?
        else {
            anyhow::bail!("server did not acknowledge host registration");
        };
        anyhow::ensure!(
            confirmed == host_id,
            "server confirmed a different host identity"
        );
        Ok(Self {
            socket,
            host_id: confirmed,
        })
    }

    /// Receive server requests while keeping the host registration live.
    pub async fn run<F>(
        &mut self,
        mut execute: impl FnMut(ExecutionRequest) -> F,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = ExecutionResult> + Send,
    {
        let mut ticks = tokio::time::interval(HOST_HEARTBEAT_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut heartbeat_pending = false;
        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    anyhow::ensure!(!heartbeat_pending, "server missed a host heartbeat");
                    self.send(HostMessage::Heartbeat).await?;
                    heartbeat_pending = true;
                }
                incoming = read_server_message(&mut self.socket) => match incoming? {
                    ServerMessage::Heartbeat => heartbeat_pending = false,
                    ServerMessage::Execute { request_id, request } => {
                        let result = execute(request).await;
                        self.send(HostMessage::ExecutionResult { request_id, result }).await?;
                    }
                    ServerMessage::Registered { .. } => anyhow::bail!("duplicate registration reply"),
                },
            }
        }
    }

    async fn send(&mut self, message: HostMessage) -> anyhow::Result<()> {
        tokio::time::timeout(
            HOST_HEARTBEAT_INTERVAL,
            self.socket
                .send(Message::text(serde_json::to_string(&message)?)),
        )
        .await??;
        Ok(())
    }

    pub async fn heartbeat(&mut self) -> anyhow::Result<()> {
        self.socket
            .send(Message::text(serde_json::to_string(
                &HostMessage::Heartbeat,
            )?))
            .await?;
        match read_server_message(&mut self.socket).await? {
            ServerMessage::Heartbeat => Ok(()),
            _ => anyhow::bail!("unexpected response to host heartbeat"),
        }
    }
}

async fn read_server_message(
    socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> anyhow::Result<ServerMessage> {
    let message = socket
        .next()
        .await
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("host connection closed"))?;
    Ok(serde_json::from_str(message.to_text()?)?)
}
