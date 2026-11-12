//! Shared Pulse sessions on Marshal's existing Myko listener, following rship #845.
use std::sync::Arc;

use anyhow::{Context, Result};
use myko_auth::{
    AuthError, AuthenticatedIdentity, AuthorizationRequest, Authorizer, MykoAuthProvider,
    OperationKind, oauth::OAuthServer,
};
use myko_server::MykoServer;
use pulse_auth::{
    SelfOnlyUserAccess,
    server::{
        AuthConfig, AuthState,
        myko_transport::{MykoAuthService, PreparedMykoAuth},
    },
};
use pulse_multi_user::{AccessPolicy, RoomGrant, RoomId, RoomRegistration, RoomService};

pub struct AuthSettings {
    config: AuthConfig,
    postgres: String,
    ca_cert: Option<String>,
    oauth: Option<Arc<OAuthServer>>,
}
impl AuthSettings {
    pub fn from_env() -> Result<Option<Self>> {
        Self::read(|key| std::env::var(key).ok())
    }

    fn read(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let keys = [
            "MARSHAL_AUTH_ISSUER",
            "MARSHAL_AUTH_CLIENT_ID",
            "MARSHAL_AUTH_CLIENT_SECRET",
            "MARSHAL_AUTH_ORIGIN",
            "MARSHAL_AUTH_EMAIL_DOMAINS",
            "MARSHAL_AUTH_OAUTH_ISSUER",
            "MARSHAL_AUTH_OAUTH_RESOURCE",
        ];
        if !keys.iter().any(|key| get(key).is_some()) {
            return Ok(None);
        }
        let required = |key| {
            get(key)
                .filter(|value| !value.trim().is_empty())
                .with_context(|| format!("{key} is required when Marshal auth is configured"))
        };
        let config = AuthConfig {
            issuer: required("MARSHAL_AUTH_ISSUER")?,
            client_id: required("MARSHAL_AUTH_CLIENT_ID")?,
            client_secret: required("MARSHAL_AUTH_CLIENT_SECRET")?,
            origin: required("MARSHAL_AUTH_ORIGIN")?,
            email_domains: required("MARSHAL_AUTH_EMAIL_DOMAINS")?
                .split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .collect(),
            namespace: "marshal".into(),
        };
        anyhow::ensure!(
            !config.email_domains.is_empty(),
            "MARSHAL_AUTH_EMAIL_DOMAINS must contain an admitted domain"
        );
        let postgres = get("MARSHAL_AUTH_POSTGRES_URL")
            .or_else(|| get("MYKO_POSTGRES_URL"))
            .context("Marshal auth requires MARSHAL_AUTH_POSTGRES_URL or MYKO_POSTGRES_URL")?;
        let oauth = match (
            get("MARSHAL_AUTH_OAUTH_ISSUER"),
            get("MARSHAL_AUTH_OAUTH_RESOURCE"),
        ) {
            (None, None) => None,
            (Some(issuer), Some(resource)) => Some(Arc::new(
                OAuthServer::new(&issuer, &resource, &config.origin)?.with_dynamic_registration(),
            )),
            _ => anyhow::bail!(
                "MARSHAL_AUTH_OAUTH_ISSUER and MARSHAL_AUTH_OAUTH_RESOURCE must both be configured"
            ),
        };
        Ok(Some(Self {
            config,
            postgres,
            ca_cert: get("MARSHAL_AUTH_CA_CERT"),
            oauth,
        }))
    }
}

pub struct PreparedAuth {
    integration: PreparedMykoAuth,
    auth: Arc<AuthState>,
}
impl PreparedAuth {
    pub async fn new(settings: AuthSettings) -> Result<Self> {
        let auth = if let Some(path) = settings.ca_cert {
            let certificate = reqwest::Certificate::from_pem(
                &std::fs::read(&path)
                    .with_context(|| format!("read auth CA certificate {path}"))?,
            )?;
            let http = reqwest::Client::builder()
                .https_only(settings.config.issuer.starts_with("https://"))
                .add_root_certificate(certificate)
                .build()?;
            AuthState::new_with_http_client(
                settings.config,
                &settings.postgres,
                Default::default(),
                http,
            )
            .await?
        } else {
            AuthState::new(settings.config, &settings.postgres).await?
        };
        Ok(Self::from_state(auth, settings.oauth))
    }
    pub fn from_state(auth: Arc<AuthState>, oauth: Option<Arc<OAuthServer>>) -> Self {
        let integration = PreparedMykoAuth::from_state(auth.clone(), |authentication| {
            MykoAuthProvider::new(authentication, Arc::new(MarshalPolicy))
        })
        .with_oauth_server(oauth);
        Self { integration, auth }
    }
    pub fn provider(&self) -> Arc<MykoAuthProvider> {
        self.integration.provider()
    }

    pub fn install(self, server: &MykoServer) -> Result<AuthLifetime> {
        let service = self.integration.install(server.ctx(), SelfOnlyUserAccess)?;
        let rooms = RoomService::presence_only(MarshalPresence {
            auth: self.auth.clone(),
        });
        let registration = rooms
            .install_myko(&server.ctx())
            .map_err(anyhow::Error::msg)?;
        let mut changes = self.auth.subscribe_changes();
        let watched_rooms = rooms.clone();
        let watcher = tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                watched_rooms.invalidate_access();
            }
        });
        Ok(AuthLifetime {
            _service: service,
            _registration: registration,
            _rooms: rooms,
            watcher,
        })
    }
}

pub struct AuthLifetime {
    _service: MykoAuthService,
    _registration: RoomRegistration,
    _rooms: Arc<RoomService>,
    watcher: tokio::task::JoinHandle<()>,
}
impl Drop for AuthLifetime {
    fn drop(&mut self) {
        self.watcher.abort();
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

/// Public coordination preserves the existing trusted-agent contract. Verified
/// operator console identities can only be created through their own command.
struct MarshalPolicy;
impl Authorizer for MarshalPolicy {
    fn authorize_anonymous(&self, request: AuthorizationRequest<'_>) -> Result<(), AuthError> {
        // Myko separately checks ownership of outstanding command responses.
        if matches!(request, AuthorizationRequest::CommandResult { .. }) {
            return Ok(());
        }
        myko_auth::public_access::authorize(request)?;
        protect_operator_identity(None, request)
    }
    fn authorize(
        &self,
        identity: &AuthenticatedIdentity,
        request: AuthorizationRequest<'_>,
    ) -> Result<(), AuthError> {
        protect_operator_identity(Some(identity), request)?;
        if myko_auth::public_access::authorize(request).is_ok() {
            return Ok(());
        }
        let allowed = match request {
            AuthorizationRequest::Discover { kind, operation }
            | AuthorizationRequest::Execute {
                kind, operation, ..
            } => match kind {
                OperationKind::Query => operation == "GetAllSandboxRuns",
                OperationKind::Report => matches!(operation, "WatchUsers" | "WatchPresenceRoom"),
                OperationKind::Command => matches!(
                    operation,
                    "RegisterOperatorConsole"
                        | "CloseOperatorConsole"
                        | "ImportSandboxRun"
                        | "JoinPresenceRoom"
                        | "LeavePresenceRoom"
                ),
                OperationKind::View => false,
            },
            AuthorizationRequest::CommandResult { .. } => true,
            AuthorizationRequest::ClientEvent { .. } => false,
        };
        allowed.then_some(()).ok_or(AuthError::Forbidden)
    }
}

fn protect_operator_identity(
    identity: Option<&AuthenticatedIdentity>,
    request: AuthorizationRequest<'_>,
) -> Result<(), AuthError> {
    let (id, may_act_as_console) = match request {
        AuthorizationRequest::ClientEvent { proposed, .. } => {
            (proposed.get("id").and_then(|v| v.as_str()), false)
        }
        AuthorizationRequest::Execute {
            operation,
            arguments,
            ..
        } => match operation {
            "RegisterSession" => (
                arguments.pointer("/session/id").and_then(|v| v.as_str()),
                false,
            ),
            "SendMessage" | "BroadcastMessage" | "JoinRoom" | "LeaveRoom" | "ReadMessages"
            | "AckMessages" => (arguments.get("asSession").and_then(|v| v.as_str()), true),
            "RegisterOperatorConsole" => (
                arguments.pointer("/session/id").and_then(|v| v.as_str()),
                true,
            ),
            _ => (
                arguments.get("id").and_then(|v| v.as_str()),
                operation == "CloseOperatorConsole",
            ),
        },
        _ => (None, false),
    };
    let Some(id) = id.filter(|id| marshal_entities::is_operator_console(id)) else {
        return Ok(());
    };
    if may_act_as_console
        && identity
            .and_then(|identity| identity.user_id.as_ref())
            .is_some_and(|user| {
                id.starts_with(&marshal_entities::operator_console_prefix(user.as_ref()))
            })
    {
        return Ok(());
    }
    Err(AuthError::Forbidden)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_configuration_preserves_trusted_mode_and_partial_configuration_fails() {
        assert!(AuthSettings::read(|_| None).unwrap().is_none());
        assert!(
            AuthSettings::read(
                |key| (key == "MARSHAL_AUTH_ISSUER").then(|| "https://issuer.test".into())
            )
            .is_err()
        );
    }

    #[test]
    fn public_agent_contract_excludes_verified_operator_commands() {
        marshal_entities::link();
        assert!(
            MarshalPolicy
                .authorize_anonymous(AuthorizationRequest::CommandResult {
                    transaction: "owned-by-transport"
                })
                .is_ok()
        );
        for operation in [
            "RegisterSession",
            "SendMessage",
            "ReadMessages",
            "JoinRoom",
            "LeaveRoom",
            "SetSessionLastActivityAt",
        ] {
            assert!(
                MarshalPolicy
                    .authorize_anonymous(AuthorizationRequest::Execute {
                        kind: OperationKind::Command,
                        operation,
                        arguments: &json!({"id":"agent","asSession":"agent"})
                    })
                    .is_ok(),
                "{operation}"
            );
        }
        for operation in [
            "RegisterOperatorConsole",
            "CloseOperatorConsole",
            "ImportSandboxRun",
        ] {
            assert!(
                MarshalPolicy
                    .authorize_anonymous(AuthorizationRequest::Execute {
                        kind: OperationKind::Command,
                        operation,
                        arguments: &json!({})
                    })
                    .is_err(),
                "{operation}"
            );
        }
    }

    #[test]
    fn anonymous_agents_cannot_claim_verified_console_identity() {
        for (operation, arguments) in [
            (
                "RegisterSession",
                json!({"session":{"id":"console-user:tab"}}),
            ),
            ("SendMessage", json!({"asSession":"console-user:tab"})),
            ("DeleteSession", json!({"id":"console-user:tab"})),
            ("SetSessionLastActivityAt", json!({"id":"console-user:tab"})),
        ] {
            assert!(
                MarshalPolicy
                    .authorize_anonymous(AuthorizationRequest::Execute {
                        kind: OperationKind::Command,
                        operation,
                        arguments: &arguments
                    })
                    .is_err(),
                "{operation}"
            );
        }
    }
}
