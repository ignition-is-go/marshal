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
        match self.subscribe(thread_id).await {
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
        let (mut websocket, _) = tokio::time::timeout(
            RPC_TIMEOUT,
            connect_async_with_config(
                endpoint.as_str(),
                Some(app_server_websocket_config()),
                false,
            ),
        )
        .await??;
        initialize_app_server(&mut websocket).await?;
        let deadline = tokio::time::Instant::now() + RPC_TIMEOUT;
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
                Err(error) if tokio::time::Instant::now() < deadline => {
                    log::debug!("[codex-bridge] keeper pending for {thread_id}: {error:#}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error).context("subscribing Codex keeper"),
            }
        }
        let task = tokio::spawn(async move {
            while let Some(frame) = websocket.next().await {
                match frame? {
                    WebSocketMessage::Ping(payload) => {
                        websocket.send(WebSocketMessage::Pong(payload)).await?
                    }
                    WebSocketMessage::Close(_) => break,
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
