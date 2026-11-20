use super::*;

enum ThreadKeeper {
    Pending,
    Failed(String),
    Kept(tokio::task::JoinHandle<Result<()>>),
}

pub(super) struct ThreadKeepers {
    endpoint: AppServerEndpoint,
    threads: tokio::sync::Mutex<HashMap<String, ThreadKeeper>>,
}

impl std::fmt::Debug for ThreadKeepers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ThreadKeepers")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl ThreadKeepers {
    pub(super) fn new(endpoint: AppServerEndpoint) -> Self {
        Self {
            endpoint,
            threads: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub(super) async fn pending(&self, thread_id: &str) {
        self.threads
            .lock()
            .await
            .entry(thread_id.to_owned())
            .or_insert(ThreadKeeper::Pending);
    }

    pub(super) async fn retain(&self, thread_id: &str) -> Result<()> {
        let mut threads = self.threads.lock().await;
        if let Some(ThreadKeeper::Kept(task)) = threads.get(thread_id) {
            anyhow::ensure!(!task.is_finished(), "Codex keeper stopped for {thread_id}");
            return Ok(());
        }
        let subscribed = tokio::time::timeout(RPC_TIMEOUT, self.subscribe(thread_id))
            .await
            .context("timed out acquiring Codex keeper")
            .and_then(|result| result);
        match subscribed {
            Ok(task) => {
                threads.insert(thread_id.to_owned(), ThreadKeeper::Kept(task));
                Ok(())
            }
            Err(error) => {
                threads.insert(
                    thread_id.to_owned(),
                    ThreadKeeper::Failed(format!("{error:#}")),
                );
                Err(error)
            }
        }
    }

    async fn subscribe(&self, thread_id: &str) -> Result<tokio::task::JoinHandle<Result<()>>> {
        let AppServerEndpoint::WebSocket(endpoint) = &self.endpoint else {
            anyhow::bail!("managed keeper requires its dedicated loopback app-server");
        };
        let (mut websocket, _) = connect_async_with_config(
            endpoint.as_str(),
            Some(app_server_websocket_config()),
            false,
        )
        .await?;
        initialize_app_server(&mut websocket).await?;
        loop {
            write_rpc(
                &mut websocket,
                &json!({"id":2,"method":"thread/resume","params":{"threadId":thread_id}}),
            )
            .await?;
            match read_response(&mut websocket, 2).await {
                Ok(result) => {
                    anyhow::ensure!(
                        result.pointer("/thread/id").and_then(Value::as_str) == Some(thread_id),
                        "keeper subscribed to a different thread"
                    );
                    break;
                }
                Err(error) if error.to_string().contains("no rollout found") => {
                    log::debug!("[codex-bridge] keeper pending for {thread_id}: {error:#}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error).context("subscribing Codex keeper"),
            }
        }
        let thread_id = thread_id.to_owned();
        let task = tokio::spawn(async move {
            while let Some(frame) = websocket.next().await {
                match frame? {
                    WebSocketMessage::Ping(payload) => {
                        websocket.send(WebSocketMessage::Pong(payload)).await?
                    }
                    WebSocketMessage::Close(_) => break,
                    WebSocketMessage::Text(text) => {
                        let message: Value = serde_json::from_str(text.as_ref())?;
                        let method = message.get("method").and_then(Value::as_str);
                        if message.pointer("/params/threadId").and_then(Value::as_str)
                            == Some(thread_id.as_str())
                            && (matches!(method, Some("thread/closed" | "thread/archived"))
                                || method == Some("thread/status/changed")
                                    && message
                                        .pointer("/params/status/type")
                                        .and_then(Value::as_str)
                                        == Some("notLoaded"))
                        {
                            anyhow::bail!("kept Codex thread closed: {thread_id}");
                        }
                    }
                    _ => {}
                }
            }
            anyhow::bail!("Codex keeper connection closed")
        });
        Ok(task)
    }

    pub(super) async fn check(&self) -> Result<()> {
        for (thread_id, keeper) in self.threads.lock().await.iter() {
            match keeper {
                ThreadKeeper::Kept(task) => {
                    anyhow::ensure!(!task.is_finished(), "Codex keeper stopped for {thread_id}")
                }
                ThreadKeeper::Failed(error) => {
                    anyhow::bail!("Codex keeper failed for {thread_id}: {error}")
                }
                ThreadKeeper::Pending => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_selection_is_pending_without_connecting() {
        let keepers =
            ThreadKeepers::new(AppServerEndpoint::WebSocket("ws://127.0.0.1:9".to_owned()));
        keepers.pending("empty-thread").await;
        keepers.check().await.unwrap();
        assert!(matches!(
            keepers.threads.lock().await.get("empty-thread"),
            Some(ThreadKeeper::Pending)
        ));
    }

    #[tokio::test]
    async fn keeper_waits_for_resumable_thread_and_rejects_closed_subscription() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let (close, closed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let init: Value =
                serde_json::from_str(websocket.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(init["method"], "initialize");
            write_rpc(&mut websocket, &json!({"id":1,"result":{}}))
                .await
                .unwrap();
            let initialized: Value =
                serde_json::from_str(websocket.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(initialized["method"], "initialized");
            for resumable in [false, true] {
                let resume: Value = serde_json::from_str(
                    websocket.next().await.unwrap().unwrap().to_text().unwrap(),
                )
                .unwrap();
                assert_eq!(resume["method"], "thread/resume");
                assert_eq!(resume["params"]["threadId"], "kept-thread");
                let reply = if resumable {
                    json!({"id":2,"result":{"thread":{"id":"kept-thread"}}})
                } else {
                    json!({"id":2,"error":{"code":-32600,"message":"no rollout found"}})
                };
                write_rpc(&mut websocket, &reply).await.unwrap();
            }
            closed.await.unwrap();
            write_rpc(
                &mut websocket,
                &json!({"method":"thread/closed","params":{"threadId":"kept-thread"}}),
            )
            .await
            .unwrap();
            while websocket.next().await.is_some() {}
        });
        let keepers = ThreadKeepers::new(AppServerEndpoint::WebSocket(endpoint));
        keepers.pending("kept-thread").await;
        keepers.retain("kept-thread").await.unwrap();
        keepers.check().await.unwrap();
        close.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while keepers.check().await.is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(keepers.retain("kept-thread").await.is_err());
        server.await.unwrap();
    }
}
