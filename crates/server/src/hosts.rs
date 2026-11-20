//! Authenticated outbound host connections and their current capabilities.

use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use marshal_client::host::{
    ExecutionRequest, ExecutionResult, HOST_PROTOCOL_VERSION, Harness, HostMessage,
    HostRegistration, ServerMessage, websocket_config,
};
use marshal_entities::{DispatchHarness, DispatchHost, DispatchHostId};
use myko::{
    command::CommandContext, prelude::EventPublishing as _, request::RequestContext,
    server::MykoServerContext,
};
use subtle::ConstantTimeEq;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, mpsc, oneshot},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        http::StatusCode,
    },
};

const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(35);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_PENDING_REQUESTS: usize = 32;

struct PendingRequest {
    request: ExecutionRequest,
    reply: oneshot::Sender<ExecutionResult>,
}

struct RegisteredHost {
    generation: uuid::Uuid,
    registration: HostRegistration,
    requests: mpsc::Sender<PendingRequest>,
}

#[derive(Clone, Default)]
pub struct HostRegistry {
    entries: Arc<Mutex<HashMap<String, RegisteredHost>>>,
    context: Option<Arc<MykoServerContext>>,
}

impl HostRegistry {
    pub fn with_roster(context: Arc<MykoServerContext>) -> Self {
        Self {
            context: Some(context),
            ..Self::default()
        }
    }

    fn publish(&self, host_id: &str, registration: &HostRegistration, connected: bool) {
        let Some(context) = self.context.as_ref() else {
            return;
        };
        let req = RequestContext::internal(
            uuid::Uuid::new_v4().to_string().into(),
            context.host_id,
            "host-roster",
        );
        let command = CommandContext::new("host-roster".into(), Arc::new(req), context.clone());
        let row = DispatchHost {
            id: DispatchHostId(Arc::from(host_id)),
            launch_harnesses: registration
                .launch_harnesses
                .iter()
                .map(|harness| match harness {
                    Harness::Claude => DispatchHarness::Claude,
                    Harness::Codex => DispatchHarness::Codex,
                })
                .collect(),
        };
        let result = if connected {
            command.emit_set(&row)
        } else {
            command.emit_del(&row)
        };
        if let Err(error) = result {
            log::warn!("could not update dispatch host roster: {error:?}");
        }
    }

    pub async fn snapshot(&self) -> HashMap<String, HostRegistration> {
        self.entries
            .lock()
            .await
            .iter()
            .map(|(id, host)| (id.clone(), host.registration.clone()))
            .collect()
    }

    /// Send an execution request to the host's current authenticated link.
    pub async fn execute(
        &self,
        host_id: &str,
        request: ExecutionRequest,
    ) -> anyhow::Result<ExecutionResult> {
        let sender = {
            let hosts = self.entries.lock().await;
            let host = hosts
                .get(host_id)
                .ok_or_else(|| anyhow::anyhow!("host is not connected"))?;
            if let ExecutionRequest::Start { harness, .. } = &request {
                anyhow::ensure!(
                    host.registration.launch_harnesses.contains(harness),
                    "host has not opted into this harness"
                );
            }
            host.requests.clone()
        };
        let (reply, result) = oneshot::channel();
        sender
            .try_send(PendingRequest { request, reply })
            .map_err(|_| anyhow::anyhow!("host request queue is unavailable or full"))?;
        Ok(tokio::time::timeout(REQUEST_TIMEOUT, result).await??)
    }
}

pub fn load_credentials(path: &std::path::Path) -> anyhow::Result<HashMap<String, String>> {
    use anyhow::Context as _;

    let contents = std::fs::read(path).context("reading host credentials file")?;
    let credentials: HashMap<String, String> =
        serde_json::from_slice(&contents).context("parsing host credentials file")?;
    anyhow::ensure!(!credentials.is_empty(), "host credentials file is empty");
    let mut tokens = std::collections::HashSet::new();
    for (id, token) in &credentials {
        anyhow::ensure!(
            !id.is_empty()
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)),
            "invalid host ID in credentials file"
        );
        anyhow::ensure!(
            token.len() >= 32 && token.bytes().all(|byte| byte.is_ascii_graphic()),
            "host tokens must have at least 32 printable ASCII characters"
        );
        anyhow::ensure!(tokens.insert(token), "each host requires a distinct token");
    }
    Ok(credentials)
}

pub async fn serve(
    listener: TcpListener,
    registry: HostRegistry,
    credentials: HashMap<String, String>,
) -> std::io::Result<()> {
    let credentials = Arc::new(credentials);
    loop {
        let (socket, _) = listener.accept().await?;
        let registry = registry.clone();
        let credentials = credentials.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(socket, &registry, &credentials).await {
                log::debug!("host connection ended: {error}");
            }
        });
    }
}

#[allow(clippy::result_large_err)] // The WebSocket handshake callback fixes the rejection type.
async fn handle(
    socket: TcpStream,
    registry: &HostRegistry,
    credentials: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let mut host_id = None;
    let mut socket = tokio::time::timeout(
        REGISTER_TIMEOUT,
        accept_hdr_async_with_config(
            socket,
            |request: &Request, response: Response| {
                host_id = authenticate(request, credentials);
                if host_id.is_some() {
                    Ok(response)
                } else {
                    Err(rejection(StatusCode::UNAUTHORIZED))
                }
            },
            Some(websocket_config()),
        ),
    )
    .await??;
    let host_id = host_id.expect("successful handshake has an authenticated host");
    let Some(HostMessage::Register(registration)) =
        read_message(&mut socket, REGISTER_TIMEOUT).await?
    else {
        anyhow::bail!("expected host registration");
    };
    anyhow::ensure!(
        registration.protocol == HOST_PROTOCOL_VERSION,
        "unsupported host protocol"
    );
    let generation = uuid::Uuid::new_v4();
    let (sender, requests) = mpsc::channel(MAX_PENDING_REQUESTS);
    {
        let mut entries = registry.entries.lock().await;
        registry.publish(&host_id, &registration, true);
        entries.insert(
            host_id.clone(),
            RegisteredHost {
                generation,
                registration,
                requests: sender,
            },
        );
    }

    let result = run_connection(&mut socket, registry, &host_id, generation, requests).await;
    let mut entries = registry.entries.lock().await;
    if entries
        .get(&host_id)
        .is_some_and(|host| host.generation == generation)
    {
        if let Some(host) = entries.remove(&host_id) {
            registry.publish(&host_id, &host.registration, false);
        }
    }
    result
}

async fn run_connection(
    socket: &mut WebSocketStream<TcpStream>,
    registry: &HostRegistry,
    host_id: &str,
    generation: uuid::Uuid,
    mut requests: mpsc::Receiver<PendingRequest>,
) -> anyhow::Result<()> {
    send_message(
        socket,
        ServerMessage::Registered {
            host_id: host_id.into(),
        },
    )
    .await?;
    let mut pending = HashMap::<String, oneshot::Sender<ExecutionResult>>::new();
    let mut heartbeat_deadline = tokio::time::Instant::now() + HEARTBEAT_TIMEOUT;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(heartbeat_deadline) => anyhow::bail!("host heartbeat timed out"),
            incoming = read_message(socket, HEARTBEAT_TIMEOUT) => {
                let Some(message) = incoming? else { return Ok(()); };
                ensure_current(registry, host_id, generation).await?;
                match message {
                    HostMessage::Heartbeat => {
                        heartbeat_deadline = tokio::time::Instant::now() + HEARTBEAT_TIMEOUT;
                        send_message(socket, ServerMessage::Heartbeat).await?;
                    }
                    HostMessage::ExecutionResult { request_id, result } => {
                        if let Some(reply) = pending.remove(&request_id) {
                            let _ = reply.send(result);
                        }
                    }
                    HostMessage::Register(_) => anyhow::bail!("host registered twice"),
                }
            }
            request = requests.recv() => {
                let Some(PendingRequest { request, reply }) = request else { return Ok(()); };
                ensure_current(registry, host_id, generation).await?;
                pending.retain(|_, pending_reply| !pending_reply.is_closed());
                if !reply.is_closed() && pending.len() < MAX_PENDING_REQUESTS {
                    let request_id = uuid::Uuid::new_v4().to_string();
                    send_message(socket, ServerMessage::Execute { request_id: request_id.clone(), request }).await?;
                    pending.insert(request_id, reply);
                }
            }
        }
    }
}

async fn ensure_current(
    registry: &HostRegistry,
    host_id: &str,
    generation: uuid::Uuid,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        registry
            .entries
            .lock()
            .await
            .get(host_id)
            .is_some_and(|host| host.generation == generation),
        "host connection was replaced",
    );
    Ok(())
}

async fn read_message(
    socket: &mut WebSocketStream<TcpStream>,
    timeout: Duration,
) -> anyhow::Result<Option<HostMessage>> {
    match tokio::time::timeout(timeout, socket.next())
        .await?
        .transpose()?
    {
        Some(Message::Text(message)) => Ok(Some(serde_json::from_str(message.as_str())?)),
        None | Some(Message::Close(_)) => Ok(None),
        _ => anyhow::bail!("unexpected host frame"),
    }
}

async fn send_message(
    socket: &mut WebSocketStream<TcpStream>,
    message: ServerMessage,
) -> anyhow::Result<()> {
    tokio::time::timeout(
        SEND_TIMEOUT,
        socket.send(Message::text(serde_json::to_string(&message)?)),
    )
    .await??;
    Ok(())
}

fn authenticate(request: &Request, credentials: &HashMap<String, String>) -> Option<String> {
    if request.uri().path() != "/host" {
        return None;
    }
    let id = request.headers().get("x-marshal-host")?.to_str().ok()?;
    let supplied = request
        .headers()
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    let expected = credentials.get(id)?;
    (!expected.is_empty() && bool::from(expected.as_bytes().ct_eq(supplied.as_bytes())))
        .then(|| id.to_owned())
}

fn rejection(status: StatusCode) -> ErrorResponse {
    let mut response = ErrorResponse::new(None);
    *response.status_mut() = status;
    response
}
