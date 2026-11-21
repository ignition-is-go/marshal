//! Myko authorization for the trusted Marshal relay.
use std::sync::Arc;

mod policy;

use myko_auth::{
    AuthError, AuthenticatedIdentity, Authenticator, AuthorizationRequest, Authorizer, Credentials,
    MykoAuthProvider, OAuthResource,
};
use policy::MarshalPolicy;
/// Keep the coordination authorization policy active on a VPN-only relay
/// when browser credentials are disabled.
pub fn trusted_relay_provider() -> Arc<MykoAuthProvider> {
    Arc::new(MykoAuthProvider::new(
        Arc::new(RejectCredentials),
        Arc::new(MarshalPolicy),
    ))
}

const MCP_SCOPE: &str = "marshal:coordinate";

pub fn authenticated_mcp_provider(
    issuer: &str,
    audience: &str,
    resource: &str,
) -> Result<Arc<MykoAuthProvider>, AuthError> {
    let issuer = myko_auth0::login::normalize_issuer(issuer)?;
    let authentication = myko_auth0::Auth0Authenticator::new(&issuer, audience)?;
    scoped_mcp_provider(Arc::new(authentication), issuer, resource)
}

fn scoped_mcp_provider(
    authentication: Arc<dyn Authenticator>,
    issuer: String,
    resource: &str,
) -> Result<Arc<MykoAuthProvider>, AuthError> {
    let oauth = OAuthResource::new(resource, vec![issuer])?.with_scopes(vec![MCP_SCOPE.into()])?;
    Ok(Arc::new(
        MykoAuthProvider::new(authentication, Arc::new(AuthenticatedRelayPolicy))
            .with_mcp_oauth(oauth),
    ))
}

struct AuthenticatedRelayPolicy;

impl Authorizer for AuthenticatedRelayPolicy {
    fn authorize(
        &self,
        identity: &AuthenticatedIdentity,
        request: AuthorizationRequest<'_>,
    ) -> Result<(), AuthError> {
        if !identity.scopes.iter().any(|scope| scope == MCP_SCOPE) {
            return Err(AuthError::Forbidden);
        }
        MarshalPolicy.authorize_anonymous(request)
    }

    fn authorize_anonymous(&self, _: AuthorizationRequest<'_>) -> Result<(), AuthError> {
        Err(AuthError::Unauthenticated)
    }
}

struct RejectCredentials;

impl Authenticator for RejectCredentials {
    fn authenticate(
        &self,
        _credentials: Credentials,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AuthenticatedIdentity, AuthError>> + Send + '_>,
    > {
        Box::pin(async { Err(AuthError::Unauthenticated) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use myko_auth::{OperationKind, Principal, PrincipalKind};

    #[tokio::test]
    async fn myko_ingress_accepts_only_signed_tokens_for_the_configured_audience_and_scope() {
        use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
        use myko_server::{BlackholePersister, GatewayAccess, MykoServer};
        use serde_json::json;
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        marshal_entities::link();
        let issuer = "https://issuer.example/";
        let audience = "urn:marshal:fixture";
        let signing =
            EncodingKey::from_rsa_pem(include_bytes!("../tests/fixtures/mcp-token/test-key.pem"))
                .unwrap();
        let keys =
            serde_json::from_str(include_str!("../tests/fixtures/mcp-token/jwks.json")).unwrap();
        let authentication =
            Arc::new(myko_auth0::Auth0Authenticator::with_jwks(issuer, audience, keys).unwrap());
        let provider = scoped_mcp_provider(
            authentication,
            issuer.into(),
            "https://relay.example/myko/mcp",
        )
        .unwrap();
        let server = MykoServer::builder()
            .with_default_persister(Arc::new(BlackholePersister))
            .build();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let context = Arc::new(server.ctx());
            let info = server.server_info();
            while let Ok((stream, peer)) = listener.accept().await {
                tokio::spawn(myko_server::router::route_connection_with_auth(
                    stream,
                    peer,
                    context.clone(),
                    info.clone(),
                    GatewayAccess::Full,
                    provider.clone(),
                ));
            }
        });

        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("marshal-test-key".into());
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 300;
        let token = |audience: &str, scope: &str| {
            encode(
                &header,
                &json!({
                    "iss": issuer, "aud": audience, "sub": "fixture-agent",
                    "exp": expires, "scope": scope,
                }),
                &signing,
            )
            .unwrap()
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let query = json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":"execute", "arguments":{
                "code":"return await myko.query('GetAllSessions', {});"
            }}
        });
        let request = || {
            client
                .post(format!("http://{address}/myko/mcp"))
                .json(&query)
        };
        let admitted = request()
            .bearer_auth(token(audience, MCP_SCOPE))
            .send()
            .await
            .unwrap();
        assert_eq!(admitted.status(), 200);
        let reply: serde_json::Value = admitted.json().await.unwrap();
        assert!(
            reply["result"].is_object() && reply["result"]["isError"] != true,
            "{reply}"
        );
        let wrong_audience = request()
            .bearer_auth(token("urn:other", MCP_SCOPE))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_audience.status(), 401);
        let wrong_scope = request()
            .bearer_auth(token(audience, "other"))
            .send()
            .await
            .unwrap();
        if wrong_scope.status() == 200 {
            let reply: serde_json::Value = wrong_scope.json().await.unwrap();
            assert_eq!(reply["result"]["isError"], true, "{reply}");
        } else {
            assert_eq!(wrong_scope.status(), 403);
        }
        task.abort();
    }

    #[test]
    fn external_access_is_scoped_to_agent_operations() {
        marshal_entities::link();
        let policy = AuthenticatedRelayPolicy;
        let query = AuthorizationRequest::Discover {
            kind: OperationKind::Query,
            operation: "GetAllSessions",
        };
        let mut identity = AuthenticatedIdentity {
            user_id: None,
            principal: Principal {
                issuer: "https://issuer.example".into(),
                subject: "agent".into(),
                kind: PrincipalKind::Agent,
            },
            client_id: None,
            scopes: vec!["other".into()],
            delegation: Vec::new(),
            claims: serde_json::Value::Null,
            expires_at: std::time::SystemTime::now(),
        };
        assert_eq!(
            policy.authorize_anonymous(query),
            Err(AuthError::Unauthenticated)
        );
        assert_eq!(
            policy.authorize(&identity, query),
            Err(AuthError::Forbidden)
        );
        identity.scopes.push(MCP_SCOPE.into());
        assert!(policy.authorize(&identity, query).is_ok());
        assert!(
            policy
                .authorize(
                    &identity,
                    AuthorizationRequest::Discover {
                        kind: OperationKind::Command,
                        operation: "RegisterOperatorConsole",
                    }
                )
                .is_err()
        );
    }
}
