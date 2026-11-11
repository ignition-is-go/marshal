//! The operator listener authenticates through Pulse. The existing agent listener
//! remains private-network trusted and shares this listener's Myko context.
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{Router, routing::get};
use myko_auth::{
    AuthError, AuthenticatedIdentity, AuthorizationRequest, Authorizer, MykoAuthProvider,
    OperationKind,
};
use myko_server::{GatewayAccess, MykoServer, router::route_connection_with_auth};
use pulse_auth::{
    SelfOnlyUserAccess,
    server::{
        AuthConfig, AuthState,
        myko_transport::{MykoAuthService, PreparedMykoAuth},
    },
};
use pulse_multi_user::{AccessPolicy, RoomGrant, RoomId, RoomRegistration, RoomService};
use tokio::net::{TcpListener, TcpStream};
use tower::Service;
use tower_http::services::ServeDir;

/// Browser access is opt-in. When enabled, incomplete configuration is an
/// error; it never falls back to the trusted agent transport.
pub struct BrowserConfig {
    pub bind: SocketAddr,
    pub public_dir: PathBuf,
    pub auth: AuthConfig,
    pub postgres: String,
}
impl BrowserConfig {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(bind) = std::env::var_os("MARSHAL_BROWSER_BIND") else {
            return Ok(None);
        };
        let required = |name| {
            std::env::var(name)
                .with_context(|| format!("{name} is required when MARSHAL_BROWSER_BIND is set"))
        };
        Ok(Some(Self {
            bind: bind
                .to_str()
                .context("MARSHAL_BROWSER_BIND must be UTF-8")?
                .parse()?,
            public_dir: required("MARSHAL_UI_DIR")?.into(),
            postgres: required("MARSHAL_AUTH_POSTGRES_URL")?,
            auth: AuthConfig {
                issuer: required("MARSHAL_AUTH_ISSUER")?,
                client_id: required("MARSHAL_AUTH_CLIENT_ID")?,
                client_secret: required("MARSHAL_AUTH_CLIENT_SECRET")?,
                origin: required("MARSHAL_AUTH_ORIGIN")?,
                email_domains: required("MARSHAL_AUTH_EMAIL_DOMAINS")?
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect(),
                namespace: "marshal".into(),
            },
        }))
    }
}

pub struct Browser {
    listener: TcpListener,
    router: Router,
    provider: Arc<MykoAuthProvider>,
    prepared: PreparedMykoAuth,
    auth: Arc<AuthState>,
}
impl Browser {
    pub async fn new(config: BrowserConfig) -> Result<Self> {
        let auth = AuthState::new(config.auth, &config.postgres).await?;
        Self::with_state(config.bind, config.public_dir, auth).await
    }

    /// Shared entry for production and signed-issuer integration fixtures.
    pub async fn with_state(
        bind: SocketAddr,
        public_dir: PathBuf,
        auth: Arc<AuthState>,
    ) -> Result<Self> {
        anyhow::ensure!(
            public_dir.join("index.html").is_file(),
            "MARSHAL_UI_DIR must contain the built index.html"
        );
        let listener = TcpListener::bind(bind).await?;
        let prepared = PreparedMykoAuth::from_state(auth.clone(), |authentication| {
            MykoAuthProvider::new(authentication, Arc::new(BrowserPolicy))
        });
        let router = Router::new()
            .nest("/auth/user", auth.router())
            .route("/health", get(|| async { "ok" }))
            .fallback_service(ServeDir::new(&public_dir));
        Ok(Self {
            listener,
            router,
            provider: prepared.provider(),
            prepared,
            auth,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Called by Myko's fallible startup hook, after replay and before either
    /// transport starts accepting connections.
    pub fn install(self, server: &MykoServer) -> Result<BrowserLifetime> {
        let service = self.prepared.install(server.ctx(), SelfOnlyUserAccess)?;
        let rooms = RoomService::presence_only(MarshalPresence {
            auth: self.auth.clone(),
        });
        let registration = rooms
            .install_myko(&server.ctx())
            .map_err(anyhow::Error::msg)?;
        let mut changes = self.auth.subscribe_changes();
        let watched_rooms = rooms.clone();
        let auth_watcher = tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                watched_rooms.invalidate_access();
            }
        });
        let context = Arc::new(server.ctx());
        let info = server.server_info();
        let task = tokio::spawn(async move {
            loop {
                let (stream, peer) = match self.listener.accept().await {
                    Ok(connection) => connection,
                    Err(error) => {
                        log::error!("browser listener failed: {error}");
                        break;
                    }
                };
                let (router, provider, context, info) = (
                    self.router.clone(),
                    self.provider.clone(),
                    context.clone(),
                    info.clone(),
                );
                tokio::spawn(async move {
                    if let Err(error) =
                        dispatch(stream, peer, router, context, info, provider).await
                    {
                        log::debug!("browser connection ended: {error}");
                    }
                });
            }
        });
        Ok(BrowserLifetime {
            _service: service,
            _registration: registration,
            _rooms: rooms,
            auth_watcher,
            task,
        })
    }
}

pub struct BrowserLifetime {
    _service: MykoAuthService,
    _registration: RoomRegistration,
    _rooms: Arc<RoomService>,
    auth_watcher: tokio::task::JoinHandle<()>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for BrowserLifetime {
    fn drop(&mut self) {
        self.task.abort();
        self.auth_watcher.abort();
    }
}

struct MarshalPresence {
    auth: Arc<AuthState>,
}
impl AccessPolicy for MarshalPresence {
    fn authorize(
        &self,
        credential: &pulse_auth::OpaqueCredential,
        resource: &str,
    ) -> Result<RoomGrant, String> {
        if resource != "marshal" {
            return Err("unknown Marshal presence resource".into());
        }
        Ok(RoomGrant {
            room: RoomId("marshal:operators".into()),
            principal: self.auth.principal(credential)?,
        })
    }
}

/// The browser can observe the console and act only as its own operator console.
/// Unknown operations and all raw entity events are denied.
struct BrowserPolicy;
impl Authorizer for BrowserPolicy {
    fn authorize_anonymous(&self, _: AuthorizationRequest<'_>) -> Result<(), AuthError> {
        Err(AuthError::Unauthenticated)
    }
    fn authorize(
        &self,
        identity: &AuthenticatedIdentity,
        request: AuthorizationRequest<'_>,
    ) -> Result<(), AuthError> {
        let user = identity.user_id.as_ref().ok_or(AuthError::Forbidden)?;
        let prefix = marshal_entities::operator_console_prefix(user.as_ref());
        let own = |value: Option<&str>| value.is_some_and(|id| id.starts_with(&prefix));
        match request {
            AuthorizationRequest::Discover { kind, operation } => {
                allowed_operation(kind, operation)
                    .then_some(())
                    .ok_or(AuthError::Forbidden)
            }
            AuthorizationRequest::Execute {
                kind,
                operation,
                arguments,
            } => {
                if !allowed_operation(kind, operation) {
                    return Err(AuthError::Forbidden);
                }
                let allowed = match operation {
                    "RegisterSession" => {
                        own(arguments.pointer("/session/id").and_then(|v| v.as_str()))
                    }
                    "CloseOperatorConsole" => own(arguments.get("id").and_then(|v| v.as_str())),
                    "SendMessage" | "BroadcastMessage" | "JoinRoom" | "LeaveRoom"
                    | "ReadMessages" | "AckMessages" => {
                        own(arguments.get("asSession").and_then(|v| v.as_str()))
                    }
                    _ => true,
                };
                allowed.then_some(()).ok_or(AuthError::Forbidden)
            }
            AuthorizationRequest::CommandResult { .. } => Ok(()),
            AuthorizationRequest::ClientEvent { .. } => Err(AuthError::Forbidden),
        }
    }
}
fn allowed_operation(kind: OperationKind, operation: &str) -> bool {
    match kind {
        OperationKind::Query => matches!(
            operation,
            "GetAllSessions"
                | "GetAllMessages"
                | "GetAllRooms"
                | "GetAllRoomMembers"
                | "GetAllSessionNicknames"
                | "GetAllSandboxRuns"
        ),
        OperationKind::Command => matches!(
            operation,
            "RegisterSession"
                | "CloseOperatorConsole"
                | "SendMessage"
                | "BroadcastMessage"
                | "JoinRoom"
                | "LeaveRoom"
                | "ReadMessages"
                | "AckMessages"
                | "ImportSandboxRun"
                | "JoinPresenceRoom"
                | "LeavePresenceRoom"
        ),
        OperationKind::Report => matches!(operation, "WatchUsers" | "WatchPresenceRoom"),
        OperationKind::View => false,
    }
}

async fn dispatch(
    stream: TcpStream,
    peer: SocketAddr,
    router: Router,
    context: Arc<myko::server::MykoServerContext>,
    info: Arc<myko_server::mcp::dispatch::ServerInfo>,
    provider: Arc<MykoAuthProvider>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(path) = peek_path(&stream).await else {
        return Ok(());
    };
    stream.set_nodelay(true)?;
    if path == "/myko" || path.starts_with("/myko/") {
        route_connection_with_auth(stream, peer, context, info, GatewayAccess::Full, provider).await
    } else {
        let service = hyper::service::service_fn(move |request| {
            let mut router = router.clone();
            async move {
                let mut response = router.call(request).await?;
                response.headers_mut().insert(
                    axum::http::header::CONNECTION,
                    axum::http::HeaderValue::from_static("close"),
                );
                Ok::<_, std::convert::Infallible>(response)
            }
        });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
            .await?;
        Ok(())
    }
}

async fn peek_path(stream: &TcpStream) -> Option<String> {
    let mut bytes = [0; 8192];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let read = tokio::time::timeout_at(deadline, stream.peek(&mut bytes))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        if let Some(end) = bytes[..read].windows(2).position(|pair| pair == b"\r\n") {
            let line = std::str::from_utf8(&bytes[..end]).ok()?;
            return Some(
                line.split_whitespace()
                    .nth(1)?
                    .split('?')
                    .next()?
                    .to_owned(),
            );
        }
        if read == bytes.len() {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use myko_auth::{EventChange, Principal, PrincipalKind};
    use serde_json::json;

    fn identity(user: &str) -> AuthenticatedIdentity {
        AuthenticatedIdentity {
            user_id: Some(myko::identity::UserId(user.into())),
            principal: Principal {
                issuer: "https://issuer.example/".into(),
                subject: user.into(),
                kind: PrincipalKind::User,
            },
            client_id: None,
            scopes: vec![],
            delegation: vec![],
            claims: json!({"verified_email": "alice@fixture.test"}),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(60),
        }
    }
    #[test]
    fn browser_cannot_impersonate_agents_or_other_users() {
        let identity = identity("alice");
        for operation in [
            "SendMessage",
            "BroadcastMessage",
            "JoinRoom",
            "LeaveRoom",
            "ReadMessages",
            "AckMessages",
        ] {
            for sender in [None, Some("agent-session"), Some("console-bob:tab")] {
                let arguments = json!({"asSession": sender});
                assert_eq!(
                    BrowserPolicy.authorize(
                        &identity,
                        AuthorizationRequest::Execute {
                            kind: OperationKind::Command,
                            operation,
                            arguments: &arguments
                        }
                    ),
                    Err(AuthError::Forbidden),
                    "{operation}"
                );
            }
            let arguments = json!({"asSession": "console-alice:tab"});
            assert!(
                BrowserPolicy
                    .authorize(
                        &identity,
                        AuthorizationRequest::Execute {
                            kind: OperationKind::Command,
                            operation,
                            arguments: &arguments
                        }
                    )
                    .is_ok()
            );
        }
    }
    #[test]
    fn browser_cannot_inject_raw_events_or_call_unlisted_operations() {
        let identity = identity("alice");
        let data = json!({"id": "console-alice:tab"});
        for entity_type in ["Session", "Message", "PulseUser"] {
            for change in [EventChange::Set, EventChange::Delete] {
                assert_eq!(
                    BrowserPolicy.authorize(
                        &identity,
                        AuthorizationRequest::ClientEvent {
                            entity_type,
                            change,
                            proposed: &data
                        }
                    ),
                    Err(AuthError::Forbidden)
                );
            }
        }
        assert_eq!(
            BrowserPolicy.authorize(
                &identity,
                AuthorizationRequest::Execute {
                    kind: OperationKind::Command,
                    operation: "SetSessionCurrentTask",
                    arguments: &data
                }
            ),
            Err(AuthError::Forbidden)
        );
    }
    #[tokio::test]
    async fn fragmented_request_line_preserves_transport_routing() {
        use tokio::io::AsyncWriteExt;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut client = TcpStream::connect(address).await.unwrap();
            client.write_all(b"GET /my").await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            client.write_all(b"ko?query=1 HTTP/1.1\r\n").await.unwrap();
        });
        let (stream, _) = listener.accept().await.unwrap();
        assert_eq!(peek_path(&stream).await.as_deref(), Some("/myko"));
        client.await.unwrap();
    }
}
