use super::*;
use marshal_entities::{
    MANAGED_PRESENCE_SCAN, ManagedThreadRegistration, ReconcileManagedPresence,
};

pub(super) async fn run(
    endpoint: AppServerEndpoint,
    daemon: String,
    launcher: SharedLauncherState,
    evidence: Option<Arc<adoption::Evidence>>,
) {
    loop {
        if let Some(evidence) = &evidence
            && evidence.validate().is_err()
        {
            return;
        }
        let client = OwnerConnection(MykoClient::new());
        let sessions = client.watch_query::<GetAllSessions>(GetAllSessions {});
        client.set_address(Some(daemon.clone()));
        let result = match &endpoint {
            AppServerEndpoint::WebSocket(address) => {
                match tokio::time::timeout(
                    RPC_TIMEOUT,
                    connect_async_with_config(
                        address.as_str(),
                        Some(app_server_websocket_config()),
                        false,
                    ),
                )
                .await
                {
                    Ok(Ok((mut socket, _))) => {
                        observe(
                            &mut socket,
                            &client,
                            &sessions,
                            &launcher,
                            evidence.as_deref(),
                        )
                        .await
                    }
                    Ok(Err(error)) => Err(error.into()),
                    Err(error) => Err(error.into()),
                }
            }
            AppServerEndpoint::Unix(path) => {
                observe_unix(path, &client, &sessions, &launcher, evidence.as_deref()).await
            }
        };
        client.0.set_address(None);
        if let Err(error) = result {
            log::debug!("[codex-presence] observer generation ended: {error:#}");
        }
        tokio::time::sleep(RETRY_COOLDOWN).await;
    }
}

type Sessions = hyphae::Cell<Vec<Arc<Session>>, hyphae::CellImmutable>;

#[cfg(unix)]
async fn observe_unix(
    path: &Path,
    client: &MykoClient,
    sessions: &Sessions,
    launcher: &SharedLauncherState,
    evidence: Option<&adoption::Evidence>,
) -> Result<()> {
    let stream = tokio::time::timeout(RPC_TIMEOUT, tokio::net::UnixStream::connect(path)).await??;
    let request = "ws://localhost/".into_client_request()?;
    let (mut socket, _) = tokio::time::timeout(
        RPC_TIMEOUT,
        client_async_with_config(request, stream, Some(app_server_websocket_config())),
    )
    .await??;
    observe(&mut socket, client, sessions, launcher, evidence).await
}

#[cfg(not(unix))]
async fn observe_unix(
    _path: &Path,
    _client: &MykoClient,
    _sessions: &Sessions,
    _launcher: &SharedLauncherState,
    _evidence: Option<&adoption::Evidence>,
) -> Result<()> {
    anyhow::bail!("Unix-domain presence endpoints are unavailable")
}

async fn observe<S>(
    socket: &mut WebSocketStream<S>,
    client: &MykoClient,
    sessions: &Sessions,
    launcher: &SharedLauncherState,
    evidence: Option<&adoption::Evidence>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    initialize_app_server(socket).await?;
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut next_scan = Instant::now();
    let mut request_id = 20_000;
    let mut sequence = 0;
    let mut last_selection = None;
    let mut last_claims: Vec<SessionId> = Vec::new();
    let mut closed = HashSet::<String>::new();
    loop {
        tokio::select! {
            _ = poll.tick() => {},
            frame = socket.next() => {
                let frame = frame.context("presence app-server disconnected")??;
                match frame {
                    WebSocketMessage::Text(text) => {
                        let notification: Value = serde_json::from_str(&text)?;
                        if let Some(id) = closed_thread_id(&notification)
                            && relevant_thread(launcher, id).await {
                            closed.insert(id.to_owned());
                            next_scan = Instant::now();
                        }
                        if let Some(registration) = thread_registration(&notification)
                            && relevant_thread(launcher, &registration.thread_id).await {
                            closed.remove(&registration.thread_id);
                            next_scan = Instant::now();
                        }
                    },
                    WebSocketMessage::Ping(payload) => { socket.send(WebSocketMessage::Pong(payload)).await?; },
                    WebSocketMessage::Close(_) => anyhow::bail!("presence app-server closed"),
                    _ => {},
                }
            }
        }
        if let Some(evidence) = evidence {
            evidence.validate()?;
        }
        let (selection, targets, agent_id) = {
            let launcher = launcher.lock().await;
            let selection = (
                launcher.state.generation,
                launcher.selection_sequence,
                launcher.state.thread_id.clone(),
                launcher.state.kept_threads.clone(),
            );
            let mut targets = launcher.state.kept_threads.clone();
            targets.extend(launcher.state.thread_id.iter().cloned());
            (selection, targets, launcher.launch_id.clone())
        };
        closed.retain(|id| targets.contains(id));
        let missing = last_claims
            .iter()
            .any(|id| !sessions.get().iter().any(|row| &row.id == id));
        if last_selection.as_ref() == Some(&selection) && !missing && Instant::now() < next_scan {
            continue;
        }
        if !matches!(
            client.connection_status_sync(),
            ConnectionStatus::Connected(_)
        ) {
            continue;
        }
        let threads = tokio::time::timeout(
            RPC_TIMEOUT,
            scan(socket, &targets, &mut closed, &mut request_id, &agent_id),
        )
        .await
        .context("presence inventory scan timed out")??;
        if let Some(evidence) = evidence {
            evidence.validate()?;
        }
        let current = launcher.lock().await;
        if selection
            != (
                current.state.generation,
                current.selection_sequence,
                current.state.thread_id.clone(),
                current.state.kept_threads.clone(),
            )
        {
            anyhow::bail!("presence observation crossed a launcher generation");
        }
        sequence += 1;
        let command = ReconcileManagedPresence {
            sequence,
            threads,
            retire: false,
        };
        let response = client.send_command::<ReconcileManagedPresence, ()>(&command);
        drop(current);
        crate::tools::await_command(response, RPC_TIMEOUT)
            .await
            .map_err(anyhow::Error::msg)?;
        last_claims = command
            .threads
            .iter()
            .map(|thread| thread.session_id.clone())
            .collect();
        if let Some(evidence) = evidence {
            evidence.validate()?;
        }
        let current = launcher.lock().await;
        anyhow::ensure!(
            selection
                == (
                    current.state.generation,
                    current.selection_sequence,
                    current.state.thread_id.clone(),
                    current.state.kept_threads.clone()
                ),
            "presence publish crossed a launcher generation"
        );
        drop(current);
        last_selection = Some(selection);
        next_scan = Instant::now() + MANAGED_PRESENCE_SCAN;
    }
}

struct OwnerConnection(MykoClient);
impl std::ops::Deref for OwnerConnection {
    type Target = MykoClient;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Drop for OwnerConnection {
    fn drop(&mut self) {
        self.0.set_address(None);
    }
}

pub(super) struct ObserverTask(pub(super) tokio::task::JoinHandle<()>);
impl Drop for ObserverTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn scan<S>(
    socket: &mut WebSocketStream<S>,
    targets: &HashSet<String>,
    closed: &mut HashSet<String>,
    request_id: &mut i64,
    agent_id: &Option<String>,
) -> Result<Vec<ManagedThreadRegistration>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    anyhow::ensure!(
        targets.len() <= 128,
        "presence target inventory exceeds limits"
    );
    closed.clear();
    let mut loaded = HashSet::new();
    let mut cursor = None::<String>;
    let mut cursors = HashSet::new();
    loop {
        let id = *request_id;
        *request_id += 1;
        write_rpc(
            socket,
            &json!({"id":id,"method":"thread/loaded/list","params":{"limit":100,"cursor":cursor}}),
        )
        .await?;
        let (page, notifications) = read_response_collecting(socket, id).await?;
        record_closes(closed, &notifications);
        let data = page
            .get("data")
            .and_then(Value::as_array)
            .context("presence loaded list omitted data")?;
        for id in data {
            loaded.insert(
                id.as_str()
                    .context("presence loaded list returned a nonidentity")?
                    .to_owned(),
            );
        }
        anyhow::ensure!(
            loaded.len() <= 4096,
            "presence loaded inventory exceeds limits"
        );
        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match &cursor {
            None => break,
            Some(cursor) => {
                anyhow::ensure!(
                    cursors.insert(cursor.clone()) && cursors.len() <= 64,
                    "presence loaded pagination did not terminate"
                );
            }
        }
    }
    let mut threads = Vec::new();
    for id in targets.intersection(&loaded) {
        if closed.contains(id) {
            continue;
        }
        let rpc_id = *request_id;
        *request_id += 1;
        write_rpc(socket, &json!({"id":rpc_id,"method":"thread/read","params":{"threadId":id,"includeTurns":false}})).await?;
        let (response, notifications) = read_response_collecting(socket, rpc_id).await?;
        record_closes(closed, &notifications);
        let thread = response
            .get("thread")
            .context("presence read omitted thread")?;
        match thread.pointer("/status/type").and_then(Value::as_str) {
            Some("notLoaded") => continue,
            Some("idle" | "active" | "systemError") => {}
            _ => anyhow::bail!("presence requires an explicit loaded thread status"),
        }
        let registration =
            thread_value_registration(thread).context("presence read omitted identity or cwd")?;
        anyhow::ensure!(
            &registration.thread_id == id,
            "presence read returned another thread"
        );
        threads.push(ManagedThreadRegistration {
            session_id: SessionId(Arc::from(registration.session_id)),
            thread_id: registration.thread_id,
            cwd: registration.cwd,
            title: registration.name,
            agent_id: agent_id.clone(),
            operator: Some(crate::codex_hook::operator()),
            host: Some(marshal_entities::HostInfo {
                name: short_host(),
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
            }),
        });
    }
    threads.retain(|thread| !closed.contains(&thread.thread_id));
    Ok(threads)
}

fn record_closes(closed: &mut HashSet<String>, notifications: &[Value]) {
    for notification in notifications {
        if let Some(id) = closed_thread_id(notification) {
            closed.insert(id.to_owned());
        }
    }
}

async fn relevant_thread(launcher: &SharedLauncherState, id: &str) -> bool {
    let launcher = launcher.lock().await;
    launcher.state.thread_id.as_deref() == Some(id) || launcher.state.kept_threads.contains(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    async fn inventory(
        loaded: Vec<&str>,
        targets: &[&str],
        close_during_second_read: bool,
        status: Option<&str>,
    ) -> (Result<Vec<ManagedThreadRegistration>>, Vec<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let loaded: Vec<String> = loaded.into_iter().map(str::to_owned).collect();
        let status = status.map(str::to_owned);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let mut reads = Vec::new();
            while let Some(Ok(WebSocketMessage::Text(text))) = socket.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "thread/loaded/list" => json!({"data":loaded,"nextCursor":null}),
                    "thread/read" => {
                        let id = request["params"]["threadId"].as_str().unwrap();
                        assert_eq!(request["params"]["includeTurns"], false);
                        reads.push(id.to_owned());
                        if close_during_second_read && reads.len() == 2 {
                            socket.send(WebSocketMessage::Text(json!({"method":"thread/closed","params":{"threadId":reads[0]}}).to_string().into())).await.unwrap();
                        }
                        let mut row = json!({"id":id,"sessionId":id,"cwd":"/repo"});
                        if let Some(status) = &status {
                            row["status"] = json!({"type":status});
                        }
                        json!({"thread":row})
                    }
                    method => panic!("presence sent a mutating or unsupported RPC: {method}"),
                };
                socket
                    .send(WebSocketMessage::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
            reads
        });
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let targets = targets.iter().map(|id| (*id).to_owned()).collect();
        let result = scan(
            &mut socket,
            &targets,
            &mut HashSet::new(),
            &mut 1,
            &Some("selected-launch".into()),
        )
        .await;
        socket.close(None).await.unwrap();
        (result, server.await.unwrap())
    }

    #[tokio::test]
    async fn empty_loaded_inventory_never_reads_cached_selected_history() {
        let (result, reads) = inventory(vec![], &["cached-selected"], false, Some("idle")).await;
        assert!(result.unwrap().is_empty());
        assert!(reads.is_empty());
        let (result, reads) = inventory(
            vec!["selected", "unrelated"],
            &["selected"],
            false,
            Some("idle"),
        )
        .await;
        assert_eq!(
            result
                .unwrap()
                .iter()
                .map(|row| row.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["selected"]
        );
        assert_eq!(reads, vec!["selected"]);
    }

    #[tokio::test]
    async fn close_during_later_read_removes_the_earlier_observation() {
        let (result, reads) = inventory(
            vec!["selected", "retained"],
            &["selected", "retained"],
            true,
            Some("idle"),
        )
        .await;
        let rows = result.unwrap();
        assert_eq!(reads.len(), 2);
        assert_eq!(
            rows.iter()
                .map(|row| row.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec![reads[1].as_str()]
        );
    }

    #[tokio::test]
    async fn unknown_loaded_status_is_not_presence_evidence() {
        let (result, _) = inventory(vec!["selected"], &["selected"], false, None).await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("explicit loaded thread status")
        );
        let (result, _) =
            inventory(vec!["selected"], &["selected"], false, Some("notLoaded")).await;
        assert!(result.unwrap().is_empty());
    }
}
