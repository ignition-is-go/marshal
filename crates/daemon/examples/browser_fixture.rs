//! Disposable signed-issuer fixture using the production shared Myko integration.
use anyhow::{Context, Result};
use daemon::auth::PreparedAuth;
use myko_server::{BlackholePersister, MykoServer};
use pulse_auth::server::{AuthConfig, AuthState};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let required = |name| std::env::var(name).with_context(|| format!("missing {name}"));
    marshal_entities::link();
    daemon::link();
    let auth = AuthState::in_memory(AuthConfig {
        issuer: required("MARSHAL_AUTH_ISSUER")?,
        client_id: required("MARSHAL_AUTH_CLIENT_ID")?,
        client_secret: required("MARSHAL_AUTH_CLIENT_SECRET")?,
        origin: required("MARSHAL_AUTH_ORIGIN")?,
        email_domains: vec!["fixture.test".into()],
        namespace: "marshal_test".into(),
    })
    .await?;
    let oauth = match (
        std::env::var("MARSHAL_AUTH_OAUTH_ISSUER"),
        std::env::var("MARSHAL_AUTH_OAUTH_RESOURCE"),
    ) {
        (Ok(issuer), Ok(resource)) => Some(Arc::new(
            myko_auth::oauth::OAuthServer::new(
                &issuer,
                &resource,
                &required("MARSHAL_AUTH_ORIGIN")?,
            )?
            .with_dynamic_registration(),
        )),
        _ => None,
    };
    let prepared = PreparedAuth::from_state(auth, oauth);
    let lifetime = Arc::new(Mutex::new(None));
    let owner = lifetime.clone();
    let server = Arc::new(
        MykoServer::builder()
            .with_bind_addr(required("MARSHAL_BIND")?.parse::<SocketAddr>()?)
            .with_auth_provider(prepared.provider())
            .with_default_persister(Arc::new(BlackholePersister))
            .try_after_init(move |server| {
                *owner.lock().unwrap() = Some(prepared.install(server)?);
                Ok(())
            })
            .build(),
    );
    let hook_address: SocketAddr = required("MARSHAL_HOOK_BIND")?.parse()?;
    let ctx = Arc::new(server.ctx());
    tokio::spawn(async move { daemon::http_listener::run(hook_address, ctx).await.unwrap() });
    tokio::spawn(daemon::cleanup::run_sweeper(server.ctx()));
    server.run().await.map_err(|error| anyhow::anyhow!(error))
}
