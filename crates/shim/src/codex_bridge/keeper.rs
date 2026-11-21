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

fn retryable_subscription_error(error: &anyhow::Error, thread_id: &str) -> bool {
    let Some(rpc) = error.downcast_ref::<AppServerRpcError>() else {
        return false;
    };
    let Some(message) = rpc.error.get("message").and_then(Value::as_str) else {
        return false;
    };
    match rpc.error.get("code").and_then(Value::as_i64) {
        Some(-32600) => {
            message == format!("no rollout found for thread id {thread_id}")
                || message
                    == format!(
                        "invalid paginated history lineage for {thread_id}: missing source rollout"
                    )
        }
        Some(-32601) => message == "list_turns is not supported yet",
        _ => false,
    }
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
                Err(error) if retryable_subscription_error(&error, thread_id) => {
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

    const THREAD_ID: &str = "01a1fbd0-cfa8-7e30-b0f5-ccbaa27034bc";
    type Peer = WebSocketStream<tokio::net::TcpStream>;

    fn lineage_error() -> Value {
        json!({"code":-32600,"message":"invalid paginated history lineage for 01a1fbd0-cfa8-7e30-b0f5-ccbaa27034bc: missing source rollout"})
    }

    fn unsupported_error() -> Value {
        json!({"code":-32601,"message":"list_turns is not supported yet"})
    }

    async fn request(websocket: &mut Peer) -> Value {
        serde_json::from_str(websocket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap()
    }

    async fn accept_keeper(listener: &tokio::net::TcpListener) -> Peer {
        let (stream, _) = listener.accept().await.unwrap();
        let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        assert_eq!(request(&mut websocket).await["method"], "initialize");
        write_rpc(&mut websocket, &json!({"id":1,"result":{}}))
            .await
            .unwrap();
        assert_eq!(request(&mut websocket).await["method"], "initialized");
        websocket
    }

    #[test]
    fn only_exact_thread_bound_rollout_errors_are_retryable() {
        let legacy = format!("no rollout found for thread id {THREAD_ID}");
        for (value, expected) in [
            (lineage_error(), true),
            (json!({"code":-32600,"message":legacy}), true),
            (json!({"code":-32603,"message":legacy}), false),
            (json!({"code":"-32600","message":legacy}), false),
            (json!({"code":-32600.0,"message":legacy}), false),
            (json!({"message":legacy}), false),
            (json!({"code":-32600,"message":null}), false),
            (json!({"code":-32600,"message":42}), false),
            (json!({"code":-32600,"message":"no rollout found"}), false),
            (
                json!({"code":-32600,"message":"no rollout found for thread id other-thread"}),
                false,
            ),
            (
                json!({"code":-32600,"message":format!("no rollout found for conversation id {THREAD_ID}")}),
                false,
            ),
            (
                json!({"code":-32600,"message":format!("{legacy} suffix")}),
                false,
            ),
            (
                json!({"code":-32600,"message":format!("prefix {legacy}")}),
                false,
            ),
            (
                json!({"code":-32600,"message":"invalid paginated history lineage for other-thread: missing source rollout"}),
                false,
            ),
            (
                json!({"code":-32600,"message":format!("invalid paginated history lineage for {THREAD_ID}: missing source rollout suffix")}),
                false,
            ),
            (
                json!({"code":-32600,"message":format!("invalid paginated history lineage for {THREAD_ID}: missing parent rollout")}),
                false,
            ),
            (Value::Null, false),
            (json!([{"code":-32600,"message":legacy}]), false),
        ] {
            let error = anyhow::Error::new(AppServerRpcError { error: value })
                .context("keeper acquisition");
            assert_eq!(
                retryable_subscription_error(&error, THREAD_ID),
                expected,
                "{error:#}"
            );
            assert!(!retryable_subscription_error(
                &error,
                "different-requested-thread"
            ));
        }
        assert!(!retryable_subscription_error(
            &anyhow::anyhow!("Codex app-server rejected request: {}", lineage_error()),
            THREAD_ID
        ));
    }

    #[test]
    fn only_exact_unsupported_turn_history_is_retryable() {
        for (value, expected) in [
            (unsupported_error(), true),
            (
                json!({"code":-32601,"message":"list_items is not supported yet"}),
                false,
            ),
            (
                json!({"code":-32601,"message":"list_turns is not supported yet suffix"}),
                false,
            ),
            (
                json!({"code":-32601,"message":"prefix list_turns is not supported yet"}),
                false,
            ),
            (
                json!({"code":-32600,"message":"list_turns is not supported yet"}),
                false,
            ),
            (
                json!({"code":"-32601","message":"list_turns is not supported yet"}),
                false,
            ),
            (
                json!({"code":-32601.0,"message":"list_turns is not supported yet"}),
                false,
            ),
            (json!({"code":-32601,"message":null}), false),
            (json!({"code":-32601,"message":42}), false),
            (json!({"message":"list_turns is not supported yet"}), false),
        ] {
            let error = anyhow::Error::new(AppServerRpcError { error: value })
                .context("keeper acquisition");
            assert_eq!(
                retryable_subscription_error(&error, THREAD_ID),
                expected,
                "{error:#}"
            );
        }
        assert!(!retryable_subscription_error(
            &anyhow::anyhow!("Codex app-server rejected request: {}", unsupported_error()),
            THREAD_ID
        ));
    }

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
        let (waiting, waiting_for_ack) = tokio::sync::oneshot::channel();
        let (acknowledge, acknowledgement) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut websocket = accept_keeper(&listener).await;
            let first = request(&mut websocket).await;
            assert_eq!(
                first,
                json!({"id":2,"method":"thread/resume","params":{"threadId":THREAD_ID}})
            );
            write_rpc(&mut websocket, &json!({"id":2,"error":lineage_error()}))
                .await
                .unwrap();
            assert_eq!(request(&mut websocket).await, first);
            write_rpc(&mut websocket, &json!({"id":2,"error":unsupported_error()}))
                .await
                .unwrap();
            assert_eq!(request(&mut websocket).await, first);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), websocket.next())
                    .await
                    .is_err()
            );
            waiting.send(()).unwrap();
            acknowledgement.await.unwrap();
            write_rpc(
                &mut websocket,
                &json!({"id":2,"result":{"thread":{"id":THREAD_ID}}}),
            )
            .await
            .unwrap();
            closed.await.unwrap();
            write_rpc(
                &mut websocket,
                &json!({"method":"thread/closed","params":{"threadId":THREAD_ID}}),
            )
            .await
            .unwrap();
            while websocket.next().await.is_some() {}
        });
        let keepers = Arc::new(ThreadKeepers::new(AppServerEndpoint::WebSocket(endpoint)));
        keepers.pending(THREAD_ID).await;
        let retaining = tokio::spawn({
            let keepers = keepers.clone();
            async move { keepers.retain(THREAD_ID).await }
        });
        waiting_for_ack.await.unwrap();
        assert!(
            !retaining.is_finished(),
            "keeper was exposed before its subscription acknowledgement"
        );
        acknowledge.send(()).unwrap();
        retaining.await.unwrap().unwrap();
        keepers.check().await.unwrap();
        close.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while keepers.check().await.is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(keepers.retain(THREAD_ID).await.is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn permanent_malformed_wrong_thread_and_transport_failures_are_not_retried() {
        for reply in [
            Some(json!({"id":2,"error":{"code":-32600,"message":"permission denied"}})),
            Some(json!({"id":2,"error":{"code":-32603,"message":lineage_error()["message"]}})),
            Some(json!({"id":2,"error":{"code":"-32600","message":lineage_error()["message"]}})),
            Some(
                json!({"id":2,"error":{"code":-32601,"message":"list_items is not supported yet"}}),
            ),
            Some(
                json!({"id":2,"error":{"code":-32600,"message":"list_turns is not supported yet"}}),
            ),
            Some(
                json!({"id":2,"error":{"code":"-32601","message":"list_turns is not supported yet"}}),
            ),
            Some(json!({"id":2,"error":null})),
            Some(json!({"id":2,"result":{"thread":{"id":"different-thread"}}})),
            None,
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("ws://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let mut websocket = accept_keeper(&listener).await;
                assert_eq!(request(&mut websocket).await["method"], "thread/resume");
                if let Some(reply) = reply {
                    write_rpc(&mut websocket, &reply).await.unwrap();
                    let next = tokio::time::timeout(Duration::from_secs(2), websocket.next())
                        .await
                        .unwrap();
                    assert!(
                        !matches!(next, Some(Ok(WebSocketMessage::Text(_)))),
                        "unexpected retry after fatal reply"
                    );
                }
                drop(websocket);
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), listener.accept())
                        .await
                        .is_err(),
                    "unexpected keeper reconnect"
                );
            });
            let keepers = ThreadKeepers::new(AppServerEndpoint::WebSocket(endpoint));
            assert!(keepers.retain(THREAD_ID).await.is_err());
            assert!(keepers.check().await.is_err());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn repeated_retryable_errors_share_the_total_initialization_and_retry_deadline() {
        assert_retry_deadline(lineage_error()).await;
    }

    #[tokio::test]
    async fn persistent_unsupported_history_never_becomes_kept_and_times_out() {
        assert_retry_deadline(unsupported_error()).await;
    }

    async fn assert_retry_deadline(rejection: Value) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert_eq!(request(&mut websocket).await["method"], "initialize");
            tokio::time::sleep(Duration::from_secs(2)).await;
            write_rpc(&mut websocket, &json!({"id":1,"result":{}}))
                .await
                .unwrap();
            assert_eq!(request(&mut websocket).await["method"], "initialized");
            let mut attempts = 0;
            while let Some(Ok(WebSocketMessage::Text(text))) = websocket.next().await {
                let resume: Value = serde_json::from_str(text.as_ref()).unwrap();
                assert_eq!(
                    resume,
                    json!({"id":2,"method":"thread/resume","params":{"threadId":THREAD_ID}})
                );
                attempts += 1;
                if write_rpc(&mut websocket, &json!({"id":2,"error":rejection}))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            attempts
        });
        let keepers = ThreadKeepers::new(AppServerEndpoint::WebSocket(endpoint));
        let started = Instant::now();
        let error = tokio::time::timeout(
            RPC_TIMEOUT + Duration::from_secs(4),
            keepers.retain(THREAD_ID),
        )
        .await
        .expect("keeper exceeded the total timeout bound")
        .unwrap_err();
        assert_eq!(error.to_string(), "timed out acquiring Codex keeper");
        assert!(started.elapsed() >= RPC_TIMEOUT);
        assert!(keepers.check().await.is_err());
        let attempts = server.await.unwrap();
        assert!(
            (2..=122).contains(&attempts),
            "unexpected retry rate: {attempts}"
        );
    }
}
