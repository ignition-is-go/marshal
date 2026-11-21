use std::sync::Arc;

use myko_auth::MykoAuthProvider;
use myko_server::{BlackholePersister, GatewayAccess, MykoServer, router};
use serde_json::json;
use tokio::{net::TcpListener, task::JoinHandle};

fn serve(
    listener: TcpListener,
    server: &MykoServer,
    provider: Arc<MykoAuthProvider>,
) -> JoinHandle<()> {
    let context = Arc::new(server.ctx());
    let info = server.server_info();
    tokio::spawn(async move {
        while let Ok((connection, peer)) = listener.accept().await {
            tokio::spawn(router::route_connection_with_auth(
                connection,
                peer,
                context.clone(),
                info.clone(),
                GatewayAccess::Full,
                provider.clone(),
            ));
        }
    })
}

#[tokio::test]
async fn mcp_challenge_is_scoped_to_the_authenticated_listener() {
    marshal_entities::link();
    let server = MykoServer::builder()
        .with_default_persister(Arc::new(BlackholePersister))
        .build();
    let trusted = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let protected = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let trusted_address = trusted.local_addr().unwrap();
    let protected_address = protected.local_addr().unwrap();

    let provider = daemon::auth::trusted_relay_provider();
    let protected_provider = daemon::auth::authenticated_mcp_provider(
        "https://login.example.test/",
        "https://relay.example.test",
        "https://relay.example.test/myko/mcp",
    )
    .unwrap();
    let trusted_task = serve(trusted, &server, provider);
    let protected_task = serve(protected, &server, protected_provider);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let query = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "execute", "arguments": {
            "code": "return await myko.query('GetAllSessions', {});"
        }}
    });
    let trusted_response = client
        .post(format!("http://{trusted_address}/myko/mcp"))
        .json(&query)
        .send()
        .await
        .unwrap();
    assert_eq!(trusted_response.status(), 200);
    let reply: serde_json::Value = trusted_response.json().await.unwrap();
    assert!(reply["error"].is_null());
    assert!(reply["result"].is_object());
    assert_ne!(reply["result"]["isError"], true);

    let denied = client
        .post(format!("http://{protected_address}/myko/mcp"))
        .json(&query)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let challenge = denied.headers()["www-authenticate"].to_str().unwrap();
    assert!(challenge.contains("resource_metadata"), "{challenge}");

    trusted_task.abort();
    protected_task.abort();
}
