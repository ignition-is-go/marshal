//! Disposable signed-issuer fixture. Uses the production browser listener and
//! agent server with an explicitly in-memory auth store. Never a daemon mode.
use anyhow::{Context, Result};
use daemon::browser::Browser;
use myko_server::{BlackholePersister, MykoServer};
use pulse_auth::server::{AuthConfig, AuthState};
use std::{
    net::SocketAddr,
    path::PathBuf,
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
    let browser = Browser::with_state(
        required("MARSHAL_BROWSER_BIND")?.parse()?,
        PathBuf::from(required("MARSHAL_UI_DIR")?),
        auth,
    )
    .await?;
    let lifetime = Arc::new(Mutex::new(None));
    let owner = lifetime.clone();
    let server = Arc::new(
        MykoServer::builder()
            .with_bind_addr(required("MARSHAL_BIND")?.parse::<SocketAddr>()?)
            .with_default_persister(Arc::new(BlackholePersister))
            .try_after_init(move |server| {
                *owner.lock().unwrap() = Some(browser.install(server)?);
                Ok(())
            })
            .build(),
    );
    let listener = tokio::net::TcpListener::bind(required("MARSHAL_HOOK_BIND")?).await?;
    let hook_address = listener.local_addr()?;
    drop(listener);
    let ctx = Arc::new(server.ctx());
    tokio::spawn(async move { daemon::http_listener::run(hook_address, ctx).await.unwrap() });
    tokio::spawn(daemon::cleanup::run_sweeper(server.ctx()));
    server.run().await.map_err(|error| anyhow::anyhow!(error))
}
