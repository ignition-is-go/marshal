//! Codex live-delivery bridge.
//!
//! Codex hooks can add context only when a turn already starts. They cannot
//! wake an idle TUI. Codex's app-server protocol *can*: a second client may
//! issue `turn/start` for a thread, and every frontend attached to that server
//! receives the resulting turn events.
//!
//! `marshal-shim codex-run` opts into that topology:
//!
//! 1. start a local Codex app-server;
//! 2. start this bridge while the TUI is alive;
//! 3. attach the TUI with `codex --remote`.
//!
//! Unix uses Codex's managed Unix-domain-socket daemon. Native Windows does not
//! have that daemon, so the launcher supervises one app-server process on an
//! ephemeral loopback-only WebSocket port and tears it down with the TUI.
//!
//! The bridge watches marshal's durable message/read views. When a direct
//! unread message targets a hook-owned session on this host, it asks the local
//! app-server to start that thread. The existing `UserPromptSubmit` hook then
//! injects and acknowledges the `<marshal_inbox>` block before the model runs.
//! Room broadcasts remain ambient, and a failed wake leaves the message unread.
//!
//! This is intentionally opt-in. A plain `codex` process owns an in-process
//! backend with no control socket, so there is nowhere safe for a peer process
//! to send `turn/start`; it retains hook-boundary inbox delivery.

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
#[cfg(windows)]
use std::{
    net::{TcpListener, TcpStream},
    thread,
};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use hyphae::Gettable;
use marshal_entities::{
    GetAllMessageReads, GetAllMessages, GetAllSessions, Message, MessageRead, Session, SessionId,
};
use myko::client::MykoClient;
use serde_json::{Value, json};
use tokio_tungstenite::{WebSocketStream, connect_async, tungstenite::Message as WebSocketMessage};
#[cfg(unix)]
use tokio_tungstenite::{client_async, tungstenite::client::IntoClientRequest};

const DEFAULT_POLL: Duration = Duration::from_millis(750);
const STARTED_COOLDOWN: Duration = Duration::from_secs(30);
const RETRY_COOLDOWN: Duration = Duration::from_secs(2);
const RPC_TIMEOUT: Duration = Duration::from_secs(8);
#[cfg(windows)]
const APP_SERVER_START_TIMEOUT: Duration = Duration::from_secs(10);
const WAKE_PROMPT: &str = "A direct message from a sibling agent arrived through Marshal. \
Process the <marshal_inbox> block injected into this turn, act only within your existing task \
and authority, reply to the sender when useful, and then continue. If no <marshal_inbox> block \
was injected, read your unread direct Marshal messages before proceeding.";

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppServerEndpoint {
    Unix(PathBuf),
    WebSocket(String),
}

impl std::fmt::Display for AppServerEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(formatter, "unix://{}", path.display()),
            Self::WebSocket(url) => formatter.write_str(url),
        }
    }
}

#[derive(Debug)]
struct BridgeArgs {
    daemon: String,
    app_server: AppServerEndpoint,
    poll: Duration,
    wake_thread: Option<SessionId>,
}

struct LaunchedAppServer {
    remote: String,
    bridge_args: Vec<String>,
    child: Option<Child>,
}

/// Launch a normal interactive Codex TUI against a shared local app-server while
/// a local bridge watches for direct Marshal messages.
pub fn run_codex(args: &[String]) -> Result<()> {
    if matches!(
        args.first().map(String::as_str),
        Some("-h") | Some("--help")
    ) {
        println!(
            "usage: marshal-shim codex-run [CODEX_ARGS...]\n\
             \n\
             Starts a local Codex app-server plus the Marshal wake bridge, then\n\
             attaches the TUI to it. Direct Marshal messages can start an idle\n\
             turn; plain Codex remains inbox-at-next-boundary.\n\
             \n\
             Set MARSHAL_CODEX_BIN to override the `codex` executable."
        );
        return Ok(());
    }

    let codex = std::env::var("MARSHAL_CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
    let mut app_server = launch_app_server(&codex)?;

    let this = std::env::current_exe().context("locating marshal-shim executable")?;
    let bridge = Command::new(this)
        .arg("codex-bridge")
        .args(&app_server.bridge_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting Codex live-delivery bridge");
    let mut bridge = match bridge {
        Ok(bridge) => bridge,
        Err(error) => {
            stop_owned_app_server(&mut app_server);
            return Err(error);
        }
    };

    let status = Command::new(&codex)
        .args(["--remote", &app_server.remote])
        .args(args)
        .status()
        .with_context(|| format!("running `{codex} --remote {}`", app_server.remote));

    // The bridge is scoped to this interactive launcher. Multiple launchers may
    // coexist; duplicate wake attempts are harmless because app-server accepts
    // only one active turn per thread.
    let _ = bridge.kill();
    let _ = bridge.wait();
    stop_owned_app_server(&mut app_server);

    let status = status?;
    if !status.success() {
        anyhow::bail!("Codex exited with {status}");
    }
    Ok(())
}

fn stop_owned_app_server(app_server: &mut LaunchedAppServer) {
    if let Some(child) = app_server.child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(unix)]
fn launch_app_server(codex: &str) -> Result<LaunchedAppServer> {
    let daemon_status = Command::new(codex)
        .args(["app-server", "daemon", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .with_context(|| format!("starting managed Codex app-server via `{codex}`"))?;
    if !daemon_status.success() {
        anyhow::bail!(
            "`{codex} app-server daemon start` exited with {daemon_status}; \
             Codex live delivery was not started"
        );
    }
    Ok(LaunchedAppServer {
        remote: "unix://".to_string(),
        bridge_args: Vec::new(),
        child: None,
    })
}

#[cfg(windows)]
fn launch_app_server(codex: &str) -> Result<LaunchedAppServer> {
    let endpoint = ephemeral_loopback_endpoint()?;
    let mut child = Command::new(codex)
        .args(["app-server", "--listen", &endpoint])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting local Codex app-server via `{codex}`"))?;
    if let Err(error) = wait_for_app_server(&mut child, &endpoint) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    Ok(LaunchedAppServer {
        remote: endpoint.clone(),
        bridge_args: vec!["--endpoint".to_string(), endpoint],
        child: Some(child),
    })
}

#[cfg(windows)]
fn ephemeral_loopback_endpoint() -> Result<String> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("reserving a local app-server port")?;
    let port = listener
        .local_addr()
        .context("reading reserved app-server port")?
        .port();
    drop(listener);
    Ok(format!("ws://127.0.0.1:{port}"))
}

#[cfg(windows)]
fn wait_for_app_server(child: &mut Child, endpoint: &str) -> Result<()> {
    let address = loopback_address(endpoint)?;
    let deadline = Instant::now() + APP_SERVER_START_TIMEOUT;
    loop {
        if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .context("checking local Codex app-server")?
        {
            anyhow::bail!("local Codex app-server exited before it was ready: {status}");
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for the local Codex app-server at {endpoint}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Run the long-lived bridge. Normally called by [`run_codex`].
pub async fn run(args: &[String]) -> Result<()> {
    let args = parse_args(args)?;
    crate::init_logging();
    if let Some(session_id) = &args.wake_thread {
        let turn_id = start_thread(&args.app_server, session_id).await?;
        println!("{turn_id}");
        return Ok(());
    }
    marshal_entities::link();

    let local_host = short_host();
    let client = Arc::new(MykoClient::new());
    let sessions = client.watch_query::<GetAllSessions>(GetAllSessions {});
    let messages = client.watch_query::<GetAllMessages>(GetAllMessages {});
    let reads = client.watch_query::<GetAllMessageReads>(GetAllMessageReads {});
    client.set_address(Some(args.daemon.clone()));

    log::info!(
        "[codex-bridge] watching host={local_host} daemon={} app_server={}",
        args.daemon,
        args.app_server
    );

    let mut cooldowns: HashMap<SessionId, Instant> = HashMap::new();
    let mut interval = tokio::time::interval(args.poll);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            signal = tokio::signal::ctrl_c() => {
                signal.context("waiting for Ctrl-C")?;
                break;
            }
        }

        let session_snapshot = sessions.get();
        let message_snapshot = messages.get();
        let read_snapshot = reads.get();
        let candidates = unread_local_codex_sessions(
            &session_snapshot,
            &message_snapshot,
            &read_snapshot,
            &local_host,
        );
        let now = Instant::now();
        cooldowns.retain(|session, until| candidates.contains(session) && *until > now);

        for session_id in candidates {
            if cooldowns.get(&session_id).is_some_and(|until| *until > now) {
                continue;
            }

            match start_thread(&args.app_server, &session_id).await {
                Ok(turn_id) => {
                    log::info!("[codex-bridge] woke thread={} turn={turn_id}", session_id.0);
                    cooldowns.insert(session_id, now + STARTED_COOLDOWN);
                }
                Err(error) => {
                    // Busy threads are retried: their normal Pre/PostToolUse
                    // hooks usually consume the inbox first. Missing threads
                    // are also harmless—this host may have a plain, non-shared
                    // Codex session on its roster.
                    log::debug!(
                        "[codex-bridge] thread={} not woken: {error:#}",
                        session_id.0
                    );
                    cooldowns.insert(session_id, now + RETRY_COOLDOWN);
                }
            }
        }
    }

    // Keep the client and watched cells live until the loop exits.
    drop((reads, messages, sessions, client));
    Ok(())
}

fn parse_args(args: &[String]) -> Result<BridgeArgs> {
    let mut daemon = None;
    let mut socket = None;
    let mut endpoint = None;
    let mut poll_ms = None;
    let mut wake_thread = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--daemon" => daemon = Some(required_value(&mut it, "--daemon")?),
            "--socket" => socket = Some(PathBuf::from(required_value(&mut it, "--socket")?)),
            "--endpoint" => endpoint = Some(required_value(&mut it, "--endpoint")?),
            "--poll-ms" => {
                let raw = required_value(&mut it, "--poll-ms")?;
                poll_ms = Some(
                    raw.parse::<u64>()
                        .with_context(|| format!("invalid --poll-ms value `{raw}`"))?,
                );
            }
            "--wake-thread" => {
                wake_thread = Some(SessionId(Arc::from(
                    required_value(&mut it, "--wake-thread")?.as_str(),
                )));
            }
            "-h" | "--help" => {
                println!(
                    "usage: marshal-shim codex-bridge [--daemon WS_URL] \
                     [--socket PATH | --endpoint LOOPBACK_WS_URL] \
                     [--poll-ms N] [--wake-thread THREAD_ID]\n\
                     \n\
                     --wake-thread is a one-shot app-server connectivity diagnostic."
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("codex-bridge: unknown argument `{other}`"),
        }
    }

    let daemon = daemon
        .or_else(crate::read_address_from_config_file)
        .or_else(|| std::env::var(crate::ADDRESS_ENV).ok())
        .or_else(|| std::env::var(crate::ADDRESS_ENV_LEGACY).ok())
        .unwrap_or_else(|| crate::DEFAULT_DAEMON_ADDRESS.to_string());
    if socket.is_some() && endpoint.is_some() {
        anyhow::bail!("codex-bridge: --socket and --endpoint are mutually exclusive");
    }
    let endpoint = endpoint.or_else(|| {
        socket
            .is_none()
            .then(|| std::env::var("MARSHAL_CODEX_APP_SERVER_ENDPOINT").ok())
            .flatten()
    });
    let app_server = if let Some(url) = endpoint {
        loopback_address(&url)?;
        AppServerEndpoint::WebSocket(url)
    } else {
        let socket = socket
            .or_else(|| std::env::var_os("MARSHAL_CODEX_APP_SERVER_SOCKET").map(PathBuf::from))
            .unwrap_or_else(default_socket);
        AppServerEndpoint::Unix(socket)
    };

    Ok(BridgeArgs {
        daemon,
        app_server,
        poll: poll_ms.map(Duration::from_millis).unwrap_or(DEFAULT_POLL),
        wake_thread,
    })
}

fn required_value<'a>(it: &mut impl Iterator<Item = &'a String>, option: &str) -> Result<String> {
    it.next()
        .cloned()
        .with_context(|| format!("{option} requires a value"))
}

fn default_socket() -> PathBuf {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"));
    codex_home
        .join("app-server-control")
        .join("app-server-control.sock")
}

fn loopback_address(endpoint: &str) -> Result<SocketAddr> {
    let authority = endpoint
        .strip_prefix("ws://")
        .context("app-server endpoint must use ws://")?;
    if authority.contains('/') || authority.contains('?') || authority.contains('#') {
        anyhow::bail!("app-server endpoint must contain only a loopback host and port");
    }
    let address = SocketAddr::from_str(authority)
        .with_context(|| format!("invalid app-server endpoint `{endpoint}`"))?;
    if !matches!(address.ip(), IpAddr::V4(ip) if ip.is_loopback())
        && !matches!(address.ip(), IpAddr::V6(ip) if ip.is_loopback())
    {
        anyhow::bail!(
            "refusing non-loopback Codex app-server endpoint `{endpoint}`; \
             the local transport is unauthenticated"
        );
    }
    Ok(address)
}

fn short_host() -> String {
    std::env::var("MARSHAL_HOST")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            gethostname::gethostname()
                .into_string()
                .ok()
                .and_then(|host| host.split('.').next().map(str::to_string))
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

/// Direct, unread, hook-owned sessions on this host. Hook-owned Codex sessions
/// have no WS `client_id`; Claude shims do. The app-server itself is the final
/// authority: a plain Codex thread is not present there and the wake request
/// simply fails while its durable inbox remains untouched.
fn unread_local_codex_sessions(
    sessions: &[Arc<Session>],
    messages: &[Arc<Message>],
    reads: &[Arc<MessageRead>],
    local_host: &str,
) -> HashSet<SessionId> {
    let local_hook_sessions: HashSet<SessionId> = sessions
        .iter()
        .filter(|session| {
            session.client_id.is_none()
                && session
                    .host
                    .as_ref()
                    .is_some_and(|host| host.name == local_host)
        })
        .map(|session| session.id.clone())
        .collect();
    let read_pairs: HashSet<(&str, &str)> = reads
        .iter()
        .map(|read| (read.message_id.0.as_ref(), read.session_id.0.as_ref()))
        .collect();

    messages
        .iter()
        .filter_map(|message| {
            let recipient = message.to_session_id.as_ref()?;
            (local_hook_sessions.contains(recipient)
                && !read_pairs.contains(&(message.id.0.as_ref(), recipient.0.as_ref())))
            .then(|| recipient.clone())
        })
        .collect()
}

async fn start_thread(app_server: &AppServerEndpoint, session_id: &SessionId) -> Result<String> {
    match app_server {
        AppServerEndpoint::WebSocket(endpoint) => {
            let (mut websocket, _) = tokio::time::timeout(RPC_TIMEOUT, connect_async(endpoint))
                .await
                .context("timed out connecting to Codex app-server")?
                .with_context(|| format!("connecting to {endpoint}"))?;
            initialize_and_start(&mut websocket, session_id).await
        }
        AppServerEndpoint::Unix(socket) => start_thread_over_unix(socket, session_id).await,
    }
}

#[cfg(unix)]
async fn start_thread_over_unix(socket: &Path, session_id: &SessionId) -> Result<String> {
    use tokio::net::UnixStream;

    let stream = tokio::time::timeout(RPC_TIMEOUT, UnixStream::connect(socket))
        .await
        .context("timed out connecting to Codex app-server")?
        .with_context(|| format!("connecting to {}", socket.display()))?;
    // Codex's Unix transport is WebSocket-over-UDS, not raw JSONL. The URI is
    // used only for the HTTP Upgrade Host/path; the already-connected Unix
    // stream determines the destination.
    let request = "ws://localhost/"
        .into_client_request()
        .context("building app-server WebSocket request")?;
    let (mut websocket, _) = tokio::time::timeout(RPC_TIMEOUT, client_async(request, stream))
        .await
        .context("timed out upgrading Codex app-server connection")?
        .context("upgrading Codex app-server Unix socket to WebSocket")?;

    initialize_and_start(&mut websocket, session_id).await
}

#[cfg(not(unix))]
async fn start_thread_over_unix(_socket: &Path, _session_id: &SessionId) -> Result<String> {
    anyhow::bail!("Unix-domain-socket app-server endpoints are unavailable on this platform")
}

async fn initialize_and_start<S>(
    websocket: &mut WebSocketStream<S>,
    session_id: &SessionId,
) -> Result<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    write_rpc(
        websocket,
        &json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "marshal-codex-bridge",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": { "experimentalApi": true },
            },
        }),
    )
    .await?;
    let _ = read_response(websocket, 1).await?;
    write_rpc(websocket, &json!({ "method": "initialized" })).await?;

    write_rpc(
        websocket,
        &json!({
            "id": 2,
            "method": "turn/start",
            "params": {
                "threadId": session_id.0.as_ref(),
                "input": [{ "type": "text", "text": WAKE_PROMPT }],
            },
        }),
    )
    .await?;
    let result = read_response(websocket, 2).await?;
    result
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("turn/start response omitted turn.id")
}

async fn write_rpc<S>(websocket: &mut WebSocketStream<S>, value: &Value) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let text = serde_json::to_string(value).context("encoding app-server request")?;
    tokio::time::timeout(
        RPC_TIMEOUT,
        websocket.send(WebSocketMessage::Text(text.into())),
    )
    .await
    .context("timed out writing app-server request")?
    .context("writing app-server request")
}

async fn read_response<S>(websocket: &mut WebSocketStream<S>, wanted_id: i64) -> Result<Value>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let frame = tokio::time::timeout(RPC_TIMEOUT, websocket.next())
            .await
            .context("timed out waiting for app-server response")?
            .context("Codex app-server closed the control connection")?
            .context("reading app-server response")?;
        let text = match frame {
            WebSocketMessage::Text(text) => text,
            WebSocketMessage::Ping(payload) => {
                websocket
                    .send(WebSocketMessage::Pong(payload))
                    .await
                    .context("answering app-server WebSocket ping")?;
                continue;
            }
            WebSocketMessage::Close(_) => {
                anyhow::bail!("Codex app-server closed the control connection")
            }
            _ => continue,
        };
        let message: Value =
            serde_json::from_str(text.as_ref()).context("decoding app-server response")?;
        if message.get("id").and_then(Value::as_i64) != Some(wanted_id) {
            continue;
        }
        if let Some(error) = message.get("error") {
            anyhow::bail!("Codex app-server rejected request: {error}");
        }
        return message
            .get("result")
            .cloned()
            .context("app-server response omitted result");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marshal_entities::{HostInfo, MessageId, MessageReadId};
    use tokio_tungstenite::accept_async;

    fn session(id: &str, host: &str, hook_owned: bool) -> Arc<Session> {
        Arc::new(Session {
            id: SessionId(Arc::from(id)),
            client_id: (!hook_owned).then(|| myko::entities::client::ClientId(Arc::from("client"))),
            pid: 1,
            cwd: "/repo".into(),
            git_branch: None,
            current_task: None,
            session_name: None,
            activity: None,
            kind: None,
            connected_at: 1,
            last_activity_at: None,
            last_tool: None,
            last_tool_at: None,
            operator: Some("operator".into()),
            host: Some(HostInfo {
                name: host.into(),
                os: "linux".into(),
                arch: "x86_64".into(),
            }),
            project: Some("repo".into()),
            channels_enabled: None,
        })
    }

    fn message(id: &str, recipient: Option<&str>) -> Arc<Message> {
        Arc::new(Message {
            id: MessageId(Arc::from(id)),
            from_session_id: SessionId(Arc::from("sender")),
            to_session_id: recipient.map(|id| SessionId(Arc::from(id))),
            to_room_id: None,
            to_operator: None,
            body: "hello".into(),
            sent_at: 2,
        })
    }

    #[test]
    fn selects_only_local_hook_owned_sessions_with_unread_direct_messages() {
        let sessions = vec![
            session("local-codex", "host-a", true),
            session("local-claude", "host-a", false),
            session("remote-codex", "host-b", true),
        ];
        let messages = vec![
            message("m-local", Some("local-codex")),
            message("m-claude", Some("local-claude")),
            message("m-remote", Some("remote-codex")),
            message("m-room", None),
        ];

        assert_eq!(
            unread_local_codex_sessions(&sessions, &messages, &[], "host-a"),
            HashSet::from([SessionId(Arc::from("local-codex"))])
        );
    }

    #[test]
    fn read_direct_message_does_not_trigger() {
        let sessions = vec![session("local-codex", "host-a", true)];
        let messages = vec![message("m-local", Some("local-codex"))];
        let reads = vec![Arc::new(MessageRead {
            id: MessageReadId(Arc::from("m-local::local-codex")),
            message_id: MessageId(Arc::from("m-local")),
            session_id: SessionId(Arc::from("local-codex")),
            read_at: 3,
        })];

        assert!(unread_local_codex_sessions(&sessions, &messages, &reads, "host-a").is_empty());
    }

    #[test]
    fn app_server_websocket_endpoint_must_be_loopback() {
        assert!(loopback_address("ws://127.0.0.1:4500").is_ok());
        assert!(loopback_address("ws://[::1]:4500").is_ok());
        assert!(loopback_address("ws://192.0.2.10:4500").is_err());
        assert!(loopback_address("wss://127.0.0.1:4500").is_err());
        assert!(loopback_address("ws://127.0.0.1:4500/path").is_err());
    }

    async fn serve_fake_app_server<S>(stream: S)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let mut websocket = accept_async(stream).await.expect("WebSocket upgrade");

        let initialize = websocket.next().await.unwrap().unwrap();
        let initialize: Value = serde_json::from_str(initialize.to_text().unwrap()).unwrap();
        assert_eq!(initialize["method"], "initialize");
        websocket
            .send(WebSocketMessage::Text(
                "{\"id\":1,\"result\":{\"userAgent\":\"test\",\"platformFamily\":\"test\",\"platformOs\":\"test\",\"codexHome\":\"/tmp\"}}"
                    .into(),
            ))
            .await
            .unwrap();

        let initialized = websocket.next().await.unwrap().unwrap();
        let initialized: Value = serde_json::from_str(initialized.to_text().unwrap()).unwrap();
        assert_eq!(initialized["method"], "initialized");

        let start = websocket.next().await.unwrap().unwrap();
        let start: Value = serde_json::from_str(start.to_text().unwrap()).unwrap();
        assert_eq!(start["method"], "turn/start");
        assert_eq!(start["params"]["threadId"], "thread-123");
        assert_eq!(start["params"]["input"][0]["text"], WAKE_PROMPT);
        websocket
            .send(WebSocketMessage::Text(
                "{\"id\":2,\"result\":{\"turn\":{\"id\":\"turn-456\"}}}".into(),
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn websocket_app_server_rpc_initializes_then_starts_the_target_thread() {
        use tokio::net::TcpListener as TokioTcpListener;

        let listener = TokioTcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake app-server");
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept bridge");
            serve_fake_app_server(stream).await;
        });
        let endpoint = AppServerEndpoint::WebSocket(format!("ws://{address}"));

        let turn = start_thread(&endpoint, &SessionId(Arc::from("thread-123")))
            .await
            .expect("wake thread");
        assert_eq!(turn, "turn-456");
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_app_server_rpc_initializes_then_starts_the_target_thread() {
        use tokio::net::UnixListener;

        let temp =
            std::env::temp_dir().join(format!("marshal-codex-{}.sock", uuid::Uuid::new_v4()));
        let listener = UnixListener::bind(&temp).expect("bind fake app-server");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept bridge");
            serve_fake_app_server(stream).await;
        });

        let endpoint = AppServerEndpoint::Unix(temp.clone());
        let turn = start_thread(&endpoint, &SessionId(Arc::from("thread-123")))
            .await
            .expect("wake thread");
        assert_eq!(turn, "turn-456");
        server.await.unwrap();
        let _ = std::fs::remove_file(temp);
    }
}
