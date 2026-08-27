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
//! 2. subscribe this bridge to app-server lifecycle events;
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
//! On Unix, per-TUI bridges attached to the shared app-server elect one
//! host-local wake leader; every bridge still observes lifecycle events.
//! A per-TUI loopback proxy observes that connection's authoritative
//! `thread/started` event or picker-issued `thread/resume` request, while the
//! bridge registers its root session id before the first prompt. `codex-run`
//! does not launch the TUI until both are ready.
//!
//! This is intentionally opt-in. A plain `codex` process owns an in-process
//! backend with no control socket, so there is nowhere safe for a peer process
//! to send `turn/start`; it retains hook-boundary inbox delivery.

#[cfg(windows)]
use std::net::{TcpListener, TcpStream};
use std::{
    collections::{HashMap, HashSet},
    fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
#[cfg(unix)]
use std::{
    fs::OpenOptions,
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use hyphae::Gettable;
use marshal_entities::{
    GetAllMessageReads, GetAllMessages, GetAllSessions, Message, MessageRead, Session, SessionId,
};
use myko::client::MykoClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use sha2::{Digest, Sha256};
use tokio_tungstenite::{
    WebSocketStream, accept_async, connect_async, tungstenite::Message as WebSocketMessage,
};
#[cfg(unix)]
use tokio_tungstenite::{client_async, tungstenite::client::IntoClientRequest};

const DEFAULT_POLL: Duration = Duration::from_millis(750);
const WAKE_COALESCE_WINDOW: Duration = Duration::from_secs(30);
const RETRY_COOLDOWN: Duration = Duration::from_secs(2);
#[cfg(unix)]
const WAKE_LEADER_SETTLE: Duration = Duration::from_secs(2);
const RPC_TIMEOUT: Duration = Duration::from_secs(8);
const BRIDGE_START_TIMEOUT: Duration = Duration::from_secs(10);
const TUI_RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const REGISTRATION_RETRY: Duration = Duration::from_secs(2);
#[cfg(windows)]
const APP_SERVER_START_TIMEOUT: Duration = Duration::from_secs(10);
const WAKE_PROMPT: &str = "Handle the injected <marshal_inbox>; if absent, read unread direct \
Marshal messages. Then continue your current task.";

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

/// Coordinates wake ownership among bridges attached to the same Unix
/// app-server and Marshal daemon. Lifecycle registration remains active in
/// every bridge; only model-turn creation is elected.
///
/// Unix `codex-run` launchers share one managed app-server, so without this
/// lock every per-TUI bridge sees the same unread row and submits the same
/// `turn/start`. Windows launchers own isolated app-servers and therefore do
/// not contend.
#[cfg(unix)]
#[derive(Debug)]
struct WakeLeadership {
    lock_path: PathBuf,
    lock: Option<fs::File>,
    acquired_at: Option<Instant>,
}

#[cfg(unix)]
impl WakeLeadership {
    fn new(app_server: &AppServerEndpoint, daemon: &str) -> Self {
        Self::at(wake_lock_path(app_server, daemon))
    }

    fn at(lock_path: PathBuf) -> Self {
        Self {
            lock_path,
            lock: None,
            acquired_at: None,
        }
    }

    fn ready(&mut self, now: Instant) -> Result<bool> {
        if self.lock.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&self.lock_path)
                .with_context(|| {
                    format!("opening wake-leader lock {}", self.lock_path.display())
                })?;
            // SAFETY: `file` owns this valid fd for at least the duration of
            // the call and remains stored in `self.lock` while leadership is
            // held. Dropping it releases the advisory lock.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(false);
                }
                return Err(error).with_context(|| {
                    format!("acquiring wake-leader lock {}", self.lock_path.display())
                });
            }
            log::info!(
                "[codex-bridge] acquired wake leadership via {}",
                self.lock_path.display()
            );
            self.lock = Some(file);
            self.acquired_at = Some(now);
        }

        Ok(self
            .acquired_at
            .is_some_and(|acquired| now.saturating_duration_since(acquired) >= WAKE_LEADER_SETTLE))
    }
}

#[cfg(unix)]
fn wake_lock_path(app_server: &AppServerEndpoint, daemon: &str) -> PathBuf {
    let identity = format!("{app_server}\n{daemon}");
    let digest = Sha256::digest(identity.as_bytes());
    let suffix: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let directory = match app_server {
        AppServerEndpoint::Unix(socket) => socket
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
        AppServerEndpoint::WebSocket(_) => std::env::temp_dir(),
    };
    directory.join(format!(".marshal-codex-wake-{suffix}.lock"))
}

#[cfg(not(unix))]
#[derive(Debug)]
struct WakeLeadership;

#[cfg(not(unix))]
impl WakeLeadership {
    fn new(_app_server: &AppServerEndpoint, _daemon: &str) -> Self {
        Self
    }

    fn ready(&mut self, _now: Instant) -> Result<bool> {
        Ok(true)
    }
}

#[derive(Debug)]
struct BridgeArgs {
    daemon: String,
    app_server: AppServerEndpoint,
    poll: Duration,
    wake_thread: Option<SessionId>,
    ready_file: Option<PathBuf>,
    launcher_state_file: Option<PathBuf>,
    launcher_cwd: Option<PathBuf>,
    launcher_thread_id: Option<String>,
    tui_proxy: Option<AppServerEndpoint>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct LauncherState {
    generation: u64,
    connected: bool,
    thread_id: Option<String>,
}

#[derive(Debug)]
struct LauncherStateWriter {
    path: PathBuf,
    cwd: PathBuf,
    state: LauncherState,
}

type SharedLauncherState = Arc<tokio::sync::Mutex<LauncherStateWriter>>;

impl LauncherStateWriter {
    fn new(path: PathBuf, cwd: PathBuf, thread_id: Option<String>) -> Self {
        Self {
            path,
            cwd,
            state: LauncherState {
                thread_id,
                ..LauncherState::default()
            },
        }
    }

    fn connected(&mut self) -> Result<()> {
        self.state.generation = self.state.generation.saturating_add(1);
        self.state.connected = true;
        self.persist()
    }

    fn disconnected(&mut self) -> Result<()> {
        self.state.connected = false;
        self.persist()
    }

    fn observe(&mut self, registration: &ThreadRegistration) -> Result<()> {
        if self.state.thread_id.is_none() && same_cwd(&self.cwd, Path::new(&registration.cwd)) {
            self.state.thread_id = Some(registration.thread_id.clone());
            self.persist()?;
        }
        Ok(())
    }

    fn selected(&mut self, thread_id: &str) -> Result<()> {
        if self.state.thread_id.is_none() {
            self.state.thread_id = Some(thread_id.to_string());
            self.persist()?;
        }
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        let encoded = serde_json::to_vec(&self.state).context("encoding codex-run state")?;
        fs::write(&self.path, encoded)
            .with_context(|| format!("writing codex-run state {}", self.path.display()))
    }
}

struct LaunchedAppServer {
    remote: String,
    bridge_args: Vec<String>,
    child: Option<Child>,
    proxy_path: Option<PathBuf>,
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
    run_codex_binary(&codex, args)
}

fn run_codex_binary(codex: &str, args: &[String]) -> Result<()> {
    let mut app_server = match launch_app_server(codex) {
        Ok(app_server) => app_server,
        Err(error) => return run_native_codex(codex, args, &error),
    };
    let ready_file = std::env::temp_dir().join(format!(
        "marshal-codex-bridge-{}.ready",
        uuid::Uuid::new_v4()
    ));
    let launcher_state_file = std::env::temp_dir().join(format!(
        "marshal-codex-launcher-{}.json",
        uuid::Uuid::new_v4()
    ));
    let cwd = std::env::current_dir().context("getting codex-run cwd")?;
    let thread_cwd = codex_thread_cwd(args, &cwd);
    let mut bridge_args = app_server.bridge_args.clone();
    bridge_args.push("--ready-file".to_string());
    bridge_args.push(ready_file.to_string_lossy().into_owned());
    bridge_args.push("--launcher-state-file".to_string());
    bridge_args.push(launcher_state_file.to_string_lossy().into_owned());
    bridge_args.push("--launcher-cwd".to_string());
    bridge_args.push(thread_cwd.to_string_lossy().into_owned());
    if let Some(thread_id) = explicit_resume_thread_id(args) {
        bridge_args.push("--launcher-thread-id".to_string());
        bridge_args.push(thread_id);
    }

    let this = std::env::current_exe().context("locating marshal-shim executable")?;
    let bridge = Command::new(this)
        .arg("codex-bridge")
        .args(&bridge_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting Codex live-delivery bridge");
    let mut bridge = match bridge {
        Ok(bridge) => bridge,
        Err(error) => {
            cleanup_proxy(&app_server);
            stop_owned_app_server(&mut app_server);
            return run_native_codex(codex, args, &error);
        }
    };
    if let Err(error) = wait_for_bridge_ready(&mut bridge, &ready_file) {
        let _ = bridge.kill();
        let _ = bridge.wait();
        let _ = fs::remove_file(&ready_file);
        let _ = fs::remove_file(&launcher_state_file);
        cleanup_proxy(&app_server);
        stop_owned_app_server(&mut app_server);
        return run_native_codex(codex, args, &error);
    }

    let mut tui_args = args.to_vec();
    let status = loop {
        let launch_generation = read_launcher_state(&launcher_state_file)
            .map(|state| state.generation)
            .unwrap_or_default();
        let mut command = codex_tui_command(codex, &app_server.remote, &tui_args, &cwd);
        let status = command
            .status()
            .with_context(|| format!("running `{codex} --remote {}`", app_server.remote))?;

        let current = read_launcher_state(&launcher_state_file);
        let already_reconnected = current
            .as_ref()
            .is_some_and(|state| state.generation > launch_generation && state.connected);
        let recovered = if already_reconnected {
            current
        } else if status.success() {
            None
        } else {
            wait_for_app_server_recovery(&mut bridge, &launcher_state_file, launch_generation)?
        };

        let Some(thread_id) = recovered.and_then(|state| state.thread_id) else {
            break status;
        };
        eprintln!("Codex app-server restarted; reconnecting and resuming thread {thread_id} ...");
        tui_args = codex_resume_args(args, &thread_id);
    };

    // The bridge is scoped to this interactive launcher. Multiple launchers may
    // coexist; Unix bridges elect one wake leader for their shared app-server,
    // while Windows launchers each own an isolated app-server.
    let _ = bridge.kill();
    let _ = bridge.wait();
    let _ = fs::remove_file(&ready_file);
    let _ = fs::remove_file(&launcher_state_file);
    cleanup_proxy(&app_server);
    stop_owned_app_server(&mut app_server);

    if !status.success() {
        anyhow::bail!("Codex exited with {status}");
    }
    Ok(())
}

fn cleanup_proxy(app_server: &LaunchedAppServer) {
    if let Some(path) = app_server.proxy_path.as_ref() {
        let _ = fs::remove_file(path);
    }
}

fn run_native_codex(codex: &str, args: &[String], bridge_error: &anyhow::Error) -> Result<()> {
    eprintln!(
        "warning: Codex live delivery is unavailable ({bridge_error:#}); starting native Codex"
    );
    let status = native_codex_command(codex, args)
        .status()
        .with_context(|| format!("running native `{codex}` after live-delivery fallback"))?;
    if !status.success() {
        anyhow::bail!("Codex exited with {status}");
    }
    Ok(())
}

fn native_codex_command(codex: &str, args: &[String]) -> Command {
    let mut command = Command::new(codex);
    command.args(args);
    command
}

fn codex_tui_command(codex: &str, remote: &str, args: &[String], cwd: &Path) -> Command {
    let mut command = Command::new(codex);
    command.args(["--remote", remote]);
    if !has_explicit_cwd(args) {
        // The managed app-server is shared across launchers and retains the
        // directory where it first started. Tell each attached TUI which
        // workspace this invocation came from so new threads do not inherit a
        // different launcher's stale cwd.
        command.arg("--cd").arg(cwd);
    }
    command.args(args);
    command
}

fn codex_resume_args(original: &[String], thread_id: &str) -> Vec<String> {
    const VALUE_OPTIONS: &[&str] = &[
        "-c",
        "--config",
        "--enable",
        "--disable",
        "--remote-auth-token-env",
        "-m",
        "--model",
        "--local-provider",
        "-p",
        "--profile",
        "-s",
        "--sandbox",
        "-C",
        "--cd",
        "--add-dir",
        "-a",
        "--ask-for-approval",
    ];
    const FLAG_OPTIONS: &[&str] = &[
        "--oss",
        "--approve-for-me",
        "--dangerously-bypass-approvals-and-sandbox",
        "--dangerously-bypass-hook-trust",
        "--search",
        "--no-alt-screen",
        // Compatibility alias used by managed launchers even when omitted
        // from a Codex version's --help output.
        "--yolo",
    ];

    let mut recovered = Vec::new();
    let mut index = 0;
    while index < original.len() {
        let argument = &original[index];
        if argument == "--" {
            break;
        }
        if VALUE_OPTIONS.contains(&argument.as_str()) {
            if let Some(value) = original.get(index + 1) {
                recovered.push(argument.clone());
                recovered.push(value.clone());
                index += 2;
                continue;
            }
        } else if FLAG_OPTIONS.contains(&argument.as_str())
            || argument.starts_with("-C") && argument.len() > 2
            || VALUE_OPTIONS
                .iter()
                .filter(|option| option.starts_with("--"))
                .any(|option| argument.starts_with(&format!("{option}=")))
        {
            recovered.push(argument.clone());
        }
        index += 1;
    }
    recovered.push("resume".to_string());
    recovered.push(thread_id.to_string());
    recovered
}

fn explicit_resume_thread_id(args: &[String]) -> Option<String> {
    const VALUE_OPTIONS: &[&str] = &[
        "-c",
        "--config",
        "--enable",
        "--disable",
        "--remote-auth-token-env",
        "-m",
        "--model",
        "--local-provider",
        "-p",
        "--profile",
        "-s",
        "--sandbox",
        "-C",
        "--cd",
        "--add-dir",
        "-a",
        "--ask-for-approval",
        "-i",
        "--image",
    ];

    let mut arguments = args
        .iter()
        .skip_while(|argument| argument.as_str() != "resume");
    arguments.next()?;
    let mut skip_value = false;
    for argument in arguments {
        if skip_value {
            skip_value = false;
            continue;
        }
        if argument == "--last" {
            return None;
        }
        if VALUE_OPTIONS.contains(&argument.as_str()) {
            skip_value = true;
            continue;
        }
        if argument.starts_with('-') {
            continue;
        }
        return uuid::Uuid::parse_str(argument)
            .ok()
            .map(|_| argument.clone());
    }
    None
}

fn read_launcher_state(path: &Path) -> Option<LauncherState> {
    let encoded = fs::read(path).ok()?;
    serde_json::from_slice(&encoded).ok()
}

fn wait_for_app_server_recovery(
    bridge: &mut Child,
    state_file: &Path,
    launch_generation: u64,
) -> Result<Option<LauncherState>> {
    let deadline = Instant::now() + TUI_RECOVERY_TIMEOUT;
    let disconnect_detection_deadline = Instant::now() + Duration::from_secs(1);
    let mut saw_disconnect = false;
    loop {
        if let Some(status) = bridge
            .try_wait()
            .context("checking Codex live-delivery bridge during app-server recovery")?
        {
            anyhow::bail!("Codex live-delivery bridge exited during app-server recovery: {status}");
        }
        if let Some(state) = read_launcher_state(state_file) {
            saw_disconnect |= !state.connected;
            if state.generation > launch_generation && state.connected && state.thread_id.is_some()
            {
                return Ok(Some(state));
            }
        }
        if !saw_disconnect && Instant::now() >= disconnect_detection_deadline {
            // A non-transport TUI failure must retain its original behavior;
            // do not turn every command/config error into a 30-second pause.
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn same_cwd(first: &Path, second: &Path) -> bool {
    #[cfg(windows)]
    {
        fn comparable(path: &Path) -> String {
            let normalized = path.to_string_lossy().replace('\\', "/");
            if let Some(unc) = normalized.strip_prefix("//?/UNC/") {
                format!("//{unc}")
            } else {
                normalized
                    .strip_prefix("//?/")
                    .unwrap_or(&normalized)
                    .to_string()
            }
        }

        comparable(first).eq_ignore_ascii_case(&comparable(second))
    }
    #[cfg(not(windows))]
    {
        first == second
    }
}

fn has_explicit_cwd(args: &[String]) -> bool {
    explicit_codex_cwd(args).is_some()
}

fn codex_thread_cwd(args: &[String], launcher_cwd: &Path) -> PathBuf {
    let selected = explicit_codex_cwd(args).unwrap_or_else(|| launcher_cwd.to_path_buf());
    let absolute = if selected.is_absolute() {
        selected
    } else {
        launcher_cwd.join(selected)
    };
    absolute.canonicalize().unwrap_or(absolute)
}

fn explicit_codex_cwd(args: &[String]) -> Option<PathBuf> {
    let mut selected = None;
    let mut arguments = args.iter().take_while(|argument| argument.as_str() != "--");
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-C" | "--cd" => {
                selected = arguments.next().map(PathBuf::from);
            }
            _ if argument.starts_with("--cd=") => {
                selected = Some(PathBuf::from(&argument[5..]));
            }
            _ if argument.starts_with("-C") && argument.len() > 2 => {
                selected = Some(PathBuf::from(&argument[2..]));
            }
            _ => {}
        }
    }
    selected
}

async fn spawn_tui_proxy(
    proxy: Option<AppServerEndpoint>,
    upstream: AppServerEndpoint,
    launcher: Option<SharedLauncherState>,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let Some(proxy) = proxy else {
        return Ok(tokio::spawn(std::future::pending::<Result<()>>()));
    };
    match (proxy, upstream) {
        #[cfg(unix)]
        (AppServerEndpoint::Unix(proxy), AppServerEndpoint::Unix(upstream)) => {
            let _ = fs::remove_file(&proxy);
            let listener = tokio::net::UnixListener::bind(&proxy)
                .with_context(|| format!("binding Codex TUI proxy {}", proxy.display()))?;
            fs::set_permissions(&proxy, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restricting Codex TUI proxy {}", proxy.display()))?;
            Ok(tokio::spawn(run_unix_tui_proxy(
                listener, upstream, launcher,
            )))
        }
        (AppServerEndpoint::WebSocket(proxy), AppServerEndpoint::WebSocket(upstream)) => {
            let address = loopback_address(&proxy)?;
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .with_context(|| format!("binding Codex TUI proxy {proxy}"))?;
            Ok(tokio::spawn(run_tcp_tui_proxy(
                listener, upstream, launcher,
            )))
        }
        _ => anyhow::bail!("Codex TUI proxy and app-server transports must match"),
    }
}

#[cfg(unix)]
async fn run_unix_tui_proxy(
    listener: tokio::net::UnixListener,
    upstream: PathBuf,
    launcher: Option<SharedLauncherState>,
) -> Result<()> {
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("accepting Codex TUI proxy client")?;
        let upstream = upstream.clone();
        let launcher = launcher.clone();
        tokio::spawn(async move {
            let result = async {
                let downstream = accept_async(stream)
                    .await
                    .context("accepting Codex TUI WebSocket")?;
                let stream = tokio::net::UnixStream::connect(&upstream)
                    .await
                    .with_context(|| format!("connecting TUI proxy to {}", upstream.display()))?;
                let request = "ws://localhost/"
                    .into_client_request()
                    .context("building TUI proxy upstream request")?;
                let (upstream, _) = client_async(request, stream)
                    .await
                    .context("upgrading TUI proxy upstream WebSocket")?;
                relay_tui_websocket(downstream, upstream, launcher).await
            }
            .await;
            if let Err(error) = result {
                log::debug!("[codex-bridge] TUI proxy connection ended: {error:#}");
            }
        });
    }
}

async fn run_tcp_tui_proxy(
    listener: tokio::net::TcpListener,
    upstream: String,
    launcher: Option<SharedLauncherState>,
) -> Result<()> {
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("accepting Codex TUI proxy client")?;
        let upstream = upstream.clone();
        let launcher = launcher.clone();
        tokio::spawn(async move {
            let result = async {
                let downstream = accept_async(stream)
                    .await
                    .context("accepting Codex TUI WebSocket")?;
                let (upstream, _) = connect_async(&upstream)
                    .await
                    .with_context(|| format!("connecting TUI proxy to {upstream}"))?;
                relay_tui_websocket(downstream, upstream, launcher).await
            }
            .await;
            if let Err(error) = result {
                log::debug!("[codex-bridge] TUI proxy connection ended: {error:#}");
            }
        });
    }
}

async fn relay_tui_websocket<Downstream, Upstream>(
    mut downstream: WebSocketStream<Downstream>,
    mut upstream: WebSocketStream<Upstream>,
    launcher: Option<SharedLauncherState>,
) -> Result<()>
where
    Downstream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    Upstream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            message = downstream.next() => {
                let message = message.context("Codex TUI closed proxy connection")??;
                if let WebSocketMessage::Text(text) = &message
                    && let Ok(value) = serde_json::from_str::<Value>(text.as_ref())
                    && let Some(thread_id) = resumed_thread_id(&value)
                    && let Some(launcher) = launcher.as_ref()
                {
                    launcher.lock().await.selected(thread_id)?;
                }
                let closed = matches!(message, WebSocketMessage::Close(_));
                upstream.send(message).await.context("forwarding TUI request to app-server")?;
                if closed {
                    return Ok(());
                }
            }
            message = upstream.next() => {
                let message = message.context("Codex app-server closed TUI proxy connection")??;
                if let WebSocketMessage::Text(text) = &message
                    && let Ok(value) = serde_json::from_str::<Value>(text.as_ref())
                    && let Some(registration) = thread_registration(&value)
                    && let Some(launcher) = launcher.as_ref()
                {
                    launcher.lock().await.observe(&registration)?;
                }
                let closed = matches!(message, WebSocketMessage::Close(_));
                downstream.send(message).await.context("forwarding app-server event to TUI")?;
                if closed {
                    return Ok(());
                }
            }
        }
    }
}

fn resumed_thread_id(message: &Value) -> Option<&str> {
    (message.get("method").and_then(Value::as_str) == Some("thread/resume"))
        .then(|| message.pointer("/params/threadId").and_then(Value::as_str))
        .flatten()
}

fn wait_for_bridge_ready(bridge: &mut Child, ready_file: &Path) -> Result<()> {
    let deadline = Instant::now() + BRIDGE_START_TIMEOUT;
    loop {
        if let Some(status) = bridge
            .try_wait()
            .context("checking Codex live-delivery bridge")?
        {
            anyhow::bail!("Codex live-delivery bridge exited before it was ready: {status}");
        }
        if ready_file.is_file() {
            fs::remove_file(ready_file).with_context(|| {
                format!("removing bridge readiness file {}", ready_file.display())
            })?;
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for Codex live-delivery bridge to subscribe to the app-server"
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
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
    let proxy_path =
        std::env::temp_dir().join(format!("marshal-codex-tui-{}.sock", uuid::Uuid::new_v4()));
    Ok(LaunchedAppServer {
        remote: format!("unix://{}", proxy_path.display()),
        bridge_args: vec![
            "--proxy-socket".to_string(),
            proxy_path.to_string_lossy().into_owned(),
        ],
        child: None,
        proxy_path: Some(proxy_path),
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

    let proxy_endpoint = ephemeral_loopback_endpoint()?;
    Ok(LaunchedAppServer {
        remote: proxy_endpoint.clone(),
        bridge_args: vec![
            "--endpoint".to_string(),
            endpoint,
            "--proxy-endpoint".to_string(),
            proxy_endpoint,
        ],
        child: Some(child),
        proxy_path: None,
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
    let hook_base = crate::codex_hook::registration_base(&args.daemon);
    let launcher = args
        .launcher_state_file
        .clone()
        .zip(args.launcher_cwd.clone())
        .map(|(path, cwd)| {
            Arc::new(tokio::sync::Mutex::new(LauncherStateWriter::new(
                path,
                cwd,
                args.launcher_thread_id.clone(),
            )))
        });
    let mut proxy_task = spawn_tui_proxy(
        args.tui_proxy.clone(),
        args.app_server.clone(),
        launcher.clone(),
    )
    .await?;
    let mut registration_task = tokio::spawn(watch_app_server_registrations(
        args.app_server.clone(),
        hook_base,
        args.ready_file.clone(),
        launcher,
    ));
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

    let mut wake_leadership = WakeLeadership::new(&args.app_server, &args.daemon);
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
            result = &mut registration_task => {
                return result
                    .context("Codex app-server registration watcher stopped")?;
            }
            result = &mut proxy_task => {
                return result
                    .context("Codex TUI proxy stopped")?;
            }
        }

        let now = Instant::now();
        match wake_leadership.ready(now) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                log::debug!("[codex-bridge] wake leadership unavailable: {error:#}");
                continue;
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
        retain_active_cooldowns(&mut cooldowns, now);

        for session_id in candidates {
            if cooldowns.get(&session_id).is_some_and(|until| *until > now) {
                continue;
            }

            match start_thread(&args.app_server, &session_id).await {
                Ok(turn_id) => {
                    log::info!("[codex-bridge] woke thread={} turn={turn_id}", session_id.0);
                    cooldowns.insert(session_id, now + WAKE_COALESCE_WINDOW);
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

    registration_task.abort();
    // Keep the client and watched cells live until the loop exits.
    drop((reads, messages, sessions, client));
    Ok(())
}

fn retain_active_cooldowns(cooldowns: &mut HashMap<SessionId, Instant>, now: Instant) {
    // Do not tie the coalescing window to the current unread snapshot. A
    // successful hook normally acknowledges the message immediately, making
    // the candidate disappear; retaining the deadline lets related messages
    // arriving moments later join the active turn instead of creating another.
    cooldowns.retain(|_, until| *until > now);
}

fn parse_args(args: &[String]) -> Result<BridgeArgs> {
    let mut daemon = None;
    let mut socket = None;
    let mut endpoint = None;
    let mut poll_ms = None;
    let mut wake_thread = None;
    let mut ready_file = None;
    let mut launcher_state_file = None;
    let mut launcher_cwd = None;
    let mut launcher_thread_id = None;
    let mut proxy_socket = None;
    let mut proxy_endpoint = None;
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
            "--ready-file" => {
                ready_file = Some(PathBuf::from(required_value(&mut it, "--ready-file")?));
            }
            "--launcher-state-file" => {
                launcher_state_file = Some(PathBuf::from(required_value(
                    &mut it,
                    "--launcher-state-file",
                )?));
            }
            "--launcher-cwd" => {
                launcher_cwd = Some(PathBuf::from(required_value(&mut it, "--launcher-cwd")?));
            }
            "--launcher-thread-id" => {
                launcher_thread_id = Some(required_value(&mut it, "--launcher-thread-id")?);
            }
            "--proxy-socket" => {
                proxy_socket = Some(PathBuf::from(required_value(&mut it, "--proxy-socket")?));
            }
            "--proxy-endpoint" => {
                proxy_endpoint = Some(required_value(&mut it, "--proxy-endpoint")?);
            }
            "-h" | "--help" => {
                println!(
                    "usage: marshal-shim codex-bridge [--daemon WS_URL] \
                     [--socket PATH | --endpoint LOOPBACK_WS_URL] \
                     [--poll-ms N] [--wake-thread THREAD_ID] [--ready-file PATH]\n\
                     \n\
                     --wake-thread is a one-shot app-server connectivity diagnostic.\n\
                     --ready-file is an internal codex-run startup handshake."
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
    if proxy_socket.is_some() && proxy_endpoint.is_some() {
        anyhow::bail!("codex-bridge: --proxy-socket and --proxy-endpoint are mutually exclusive");
    }
    let tui_proxy = if let Some(url) = proxy_endpoint {
        loopback_address(&url)?;
        Some(AppServerEndpoint::WebSocket(url))
    } else {
        proxy_socket.map(AppServerEndpoint::Unix)
    };
    if launcher_state_file.is_some() != launcher_cwd.is_some() {
        anyhow::bail!(
            "codex-bridge: --launcher-state-file and --launcher-cwd must be provided together"
        );
    }
    if launcher_thread_id.is_some() && launcher_state_file.is_none() {
        anyhow::bail!("codex-bridge: --launcher-thread-id requires --launcher-state-file");
    }

    Ok(BridgeArgs {
        daemon,
        app_server,
        poll: poll_ms.map(Duration::from_millis).unwrap_or(DEFAULT_POLL),
        wake_thread,
        ready_file,
        launcher_state_file,
        launcher_cwd,
        launcher_thread_id,
        tui_proxy,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreadRegistration {
    thread_id: String,
    session_id: String,
    cwd: String,
}

/// Maintain a subscribed app-server connection for lifecycle discovery.
///
/// `codex-run` waits for this connection's readiness marker before it launches
/// the TUI, so the TUI's initial `thread/started` notification cannot fall into
/// a startup race. Failed daemon registrations remain pending and are retried;
/// a successful registration is left to the normal hooks for liveness refresh
/// and teardown.
async fn watch_app_server_registrations(
    app_server: AppServerEndpoint,
    hook_base: String,
    mut ready_file: Option<PathBuf>,
    launcher: Option<SharedLauncherState>,
) -> Result<()> {
    let mut pending = HashMap::new();
    loop {
        let result = match &app_server {
            AppServerEndpoint::WebSocket(endpoint) => {
                let connection = tokio::time::timeout(RPC_TIMEOUT, connect_async(endpoint)).await;
                match connection {
                    Ok(Ok((mut websocket, _))) => {
                        monitor_registration_connection(
                            &mut websocket,
                            &hook_base,
                            &mut ready_file,
                            &mut pending,
                            &launcher,
                        )
                        .await
                    }
                    Ok(Err(error)) => {
                        Err(error).with_context(|| format!("connecting to {endpoint}"))
                    }
                    Err(_) => anyhow::bail!("timed out connecting to Codex app-server"),
                }
            }
            AppServerEndpoint::Unix(socket) => {
                monitor_registrations_over_unix(
                    socket,
                    &hook_base,
                    &mut ready_file,
                    &mut pending,
                    &launcher,
                )
                .await
            }
        };
        if let Err(error) = result {
            log::debug!("[codex-bridge] app-server lifecycle connection unavailable: {error:#}");
        }
        if let Some(launcher) = launcher.as_ref()
            && let Err(error) = launcher.lock().await.disconnected()
        {
            log::debug!("[codex-bridge] could not record app-server disconnect: {error:#}");
        }
        tokio::time::sleep(RETRY_COOLDOWN).await;
    }
}

#[cfg(unix)]
async fn monitor_registrations_over_unix(
    socket: &Path,
    hook_base: &str,
    ready_file: &mut Option<PathBuf>,
    pending: &mut HashMap<String, ThreadRegistration>,
    launcher: &Option<SharedLauncherState>,
) -> Result<()> {
    use tokio::net::UnixStream;

    let stream = tokio::time::timeout(RPC_TIMEOUT, UnixStream::connect(socket))
        .await
        .context("timed out connecting to Codex app-server")?
        .with_context(|| format!("connecting to {}", socket.display()))?;
    let request = "ws://localhost/"
        .into_client_request()
        .context("building app-server WebSocket request")?;
    let (mut websocket, _) = tokio::time::timeout(RPC_TIMEOUT, client_async(request, stream))
        .await
        .context("timed out upgrading Codex app-server connection")?
        .context("upgrading Codex app-server Unix socket to WebSocket")?;
    monitor_registration_connection(&mut websocket, hook_base, ready_file, pending, launcher).await
}

#[cfg(not(unix))]
async fn monitor_registrations_over_unix(
    _socket: &Path,
    _hook_base: &str,
    _ready_file: &mut Option<PathBuf>,
    _pending: &mut HashMap<String, ThreadRegistration>,
    _launcher: &Option<SharedLauncherState>,
) -> Result<()> {
    anyhow::bail!("Unix-domain-socket app-server endpoints are unavailable on this platform")
}

async fn monitor_registration_connection<S>(
    websocket: &mut WebSocketStream<S>,
    hook_base: &str,
    ready_file: &mut Option<PathBuf>,
    pending: &mut HashMap<String, ThreadRegistration>,
    launcher: &Option<SharedLauncherState>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    initialize_app_server(websocket).await?;
    if let Some(launcher) = launcher.as_ref() {
        launcher.lock().await.connected()?;
    }
    if let Some(path) = ready_file.as_ref() {
        // codex-run deliberately starts its TUI only after this marker, so the
        // initial thread/started event cannot race us and no snapshot is needed.
        // Do not make launcher readiness depend on optional thread/list support.
        fs::write(path, b"ready")
            .with_context(|| format!("writing bridge readiness file {}", path.display()))?;
        ready_file.take();
    } else {
        if let Err(error) = snapshot_loaded_threads(websocket, pending).await {
            // Thread discovery is an optimization over the authoritative lifecycle
            // notifications. Keep the subscription alive against older app-server
            // versions that do not implement thread/list or thread/read.
            log::warn!("[codex-bridge] could not snapshot loaded threads: {error:#}");
        }
        let session_ids: Vec<String> = pending.keys().cloned().collect();
        for session_id in session_ids {
            try_pending_registration(hook_base, pending, &session_id).await;
        }
    }

    let mut retry = tokio::time::interval(REGISTRATION_RETRY);
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the interval's immediate first tick; new notifications attempt
    // registration synchronously below.
    retry.tick().await;

    loop {
        tokio::select! {
            frame = websocket.next() => {
                let frame = frame
                    .context("Codex app-server closed the lifecycle connection")?
                    .context("reading Codex app-server lifecycle notification")?;
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
                        anyhow::bail!("Codex app-server closed the lifecycle connection")
                    }
                    _ => continue,
                };
                let message: Value = serde_json::from_str(text.as_ref())
                    .context("decoding app-server lifecycle notification")?;
                if let Some(registration) = thread_registration(&message) {
                    if let Some(launcher) = launcher.as_ref() {
                        launcher.lock().await.observe(&registration)?;
                    }
                    let session_id = registration.session_id.clone();
                    pending.insert(session_id.clone(), registration);
                    try_pending_registration(hook_base, pending, &session_id).await;
                } else if let Some(thread_id) = closed_thread_id(&message) {
                    pending.retain(|_, registration| registration.thread_id != thread_id);
                }
            }
            _ = retry.tick(), if !pending.is_empty() => {
                let session_ids: Vec<String> = pending.keys().cloned().collect();
                for session_id in session_ids {
                    try_pending_registration(hook_base, pending, &session_id).await;
                }
            }
        }
    }
}

/// Discover threads that were already started before this lifecycle subscriber
/// connected. Omnigent starts the app-server and TUI independently, so relying
/// only on `thread/started` leaves a real startup race. Runtime status keeps us
/// from registering historical, unloaded threads from a shared CODEX_HOME.
async fn snapshot_loaded_threads<S>(
    websocket: &mut WebSocketStream<S>,
    pending: &mut HashMap<String, ThreadRegistration>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut cursor: Option<String> = None;
    let mut request_id = 100_i64;

    loop {
        write_rpc(
            websocket,
            &json!({
                "id": request_id,
                "method": "thread/list",
                "params": {
                    "cursor": cursor,
                    "limit": 100,
                    "sortKey": "updated_at",
                },
            }),
        )
        .await?;
        let (page, notifications) = read_response_collecting(websocket, request_id).await?;
        apply_lifecycle_notifications(pending, notifications);
        request_id += 1;

        let loaded_ids: Vec<String> = page
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|thread| {
                thread.pointer("/status/type").and_then(Value::as_str) != Some("notLoaded")
            })
            .filter_map(|thread| thread.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();

        for thread_id in loaded_ids {
            write_rpc(
                websocket,
                &json!({
                    "id": request_id,
                    "method": "thread/read",
                    "params": { "threadId": thread_id, "includeTurns": false },
                }),
            )
            .await?;
            let (result, notifications) = read_response_collecting(websocket, request_id).await?;
            apply_lifecycle_notifications(pending, notifications);
            request_id += 1;
            if let Some(registration) = result.get("thread").and_then(thread_value_registration) {
                pending.insert(registration.session_id.clone(), registration);
            }
        }

        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        if cursor.is_none() {
            return Ok(());
        }
    }
}

fn apply_lifecycle_notifications(
    pending: &mut HashMap<String, ThreadRegistration>,
    notifications: Vec<Value>,
) {
    for message in notifications {
        if let Some(registration) = thread_registration(&message) {
            pending.insert(registration.session_id.clone(), registration);
        } else if let Some(thread_id) = closed_thread_id(&message) {
            pending.retain(|_, registration| registration.thread_id != thread_id);
        }
    }
}

fn thread_registration(message: &Value) -> Option<ThreadRegistration> {
    if message.get("method").and_then(Value::as_str) != Some("thread/started") {
        return None;
    }
    thread_value_registration(message.pointer("/params/thread")?)
}

fn thread_value_registration(thread: &Value) -> Option<ThreadRegistration> {
    let thread_id = thread.get("id")?.as_str()?.to_string();
    let session_id = thread
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or(&thread_id)
        .to_string();
    let cwd = thread.get("cwd")?.as_str()?.to_string();
    Some(ThreadRegistration {
        thread_id,
        session_id,
        cwd,
    })
}

fn closed_thread_id(message: &Value) -> Option<&str> {
    (message.get("method").and_then(Value::as_str) == Some("thread/closed"))
        .then(|| message.pointer("/params/threadId").and_then(Value::as_str))
        .flatten()
}

async fn try_pending_registration(
    hook_base: &str,
    pending: &mut HashMap<String, ThreadRegistration>,
    session_id: &str,
) {
    let Some(registration) = pending.get(session_id).cloned() else {
        return;
    };
    let base = hook_base.to_string();
    let registered = tokio::task::spawn_blocking(move || {
        crate::codex_hook::register_session(&base, &registration.session_id, &registration.cwd)
    })
    .await
    .unwrap_or(false);
    if registered {
        log::info!("[codex-bridge] registered thread={session_id} before first prompt");
        pending.remove(session_id);
    } else {
        log::debug!("[codex-bridge] eager registration for thread={session_id} failed; will retry");
    }
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
    initialize_app_server(websocket).await?;

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

async fn initialize_app_server<S>(websocket: &mut WebSocketStream<S>) -> Result<()>
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
    Ok(())
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

/// Read an RPC response without losing lifecycle notifications that race the
/// response. Other response IDs belong to unrelated subscribers and are
/// ignored; notifications are returned to the registration state machine.
async fn read_response_collecting<S>(
    websocket: &mut WebSocketStream<S>,
    wanted_id: i64,
) -> Result<(Value, Vec<Value>)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut notifications = Vec::new();
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
        if message.get("id").and_then(Value::as_i64) == Some(wanted_id) {
            if let Some(error) = message.get("error") {
                anyhow::bail!("Codex app-server rejected request: {error}");
            }
            let result = message
                .get("result")
                .cloned()
                .context("app-server response omitted result")?;
            return Ok((result, notifications));
        }
        if message.get("method").and_then(Value::as_str).is_some() {
            notifications.push(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marshal_entities::{HostInfo, MessageId, MessageReadId};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;
    use tokio_tungstenite::accept_async;

    #[test]
    fn detects_explicit_codex_working_directory_arguments() {
        assert!(has_explicit_cwd(&["-C".into(), "/work/one".into()]));
        assert!(has_explicit_cwd(&["-C/work/two".into()]));
        assert!(has_explicit_cwd(&["--cd".into(), "/work/three".into()]));
        assert!(has_explicit_cwd(&["--cd=/work/four".into()]));
        assert!(!has_explicit_cwd(&["resume".into(), "--last".into()]));
        assert!(!has_explicit_cwd(&[
            "--".into(),
            "--cd=/prompt-text".into(),
        ]));
    }

    #[test]
    fn recovery_tracks_codex_effective_working_directory() {
        let temp = tempfile::tempdir().expect("working-directory tempdir");
        let selected = temp.path().join("selected");
        fs::create_dir(&selected).expect("create selected directory");

        assert!(same_cwd(
            &codex_thread_cwd(&["--cd".into(), "selected".into()], temp.path()),
            &selected,
        ));
        assert!(same_cwd(&codex_thread_cwd(&[], temp.path()), temp.path()));
    }

    #[test]
    fn codex_tui_uses_the_launchers_working_directory_by_default() {
        let command = codex_tui_command(
            "codex",
            "unix://",
            &["resume".into(), "--last".into()],
            Path::new("/work/myko"),
        );
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--remote",
                "unix://",
                "--cd",
                "/work/myko",
                "resume",
                "--last"
            ]
        );
    }

    #[test]
    fn codex_tui_preserves_an_explicit_working_directory() {
        let command = codex_tui_command(
            "codex",
            "unix://",
            &["--cd=/work/override".into()],
            Path::new("/work/myko"),
        );
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--remote", "unix://", "--cd=/work/override"]);
    }

    #[test]
    fn native_fallback_preserves_the_original_codex_arguments() {
        let command = native_codex_command(
            "/managed/codex",
            &["resume".into(), "--last".into(), "--yolo".into()],
        );
        assert_eq!(command.get_program(), "/managed/codex");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["resume", "--last", "--yolo"]);
    }

    #[test]
    fn recovery_resumes_the_recorded_thread_without_replaying_prompts() {
        let args = vec![
            "--model".into(),
            "gpt-5.6-sol".into(),
            "resume".into(),
            "old-thread".into(),
            "do not replay me".into(),
            "--yolo".into(),
            "--cd=/work/pulse".into(),
            "--image".into(),
            "/tmp/old.png".into(),
        ];
        assert_eq!(
            codex_resume_args(&args, "recorded-thread"),
            [
                "--model",
                "gpt-5.6-sol",
                "--yolo",
                "--cd=/work/pulse",
                "resume",
                "recorded-thread",
            ]
        );
    }

    #[test]
    fn launcher_state_records_one_matching_thread_across_server_generations() {
        let temp = tempfile::tempdir().expect("launcher state tempdir");
        let path = temp.path().join("state.json");
        let mut writer = LauncherStateWriter::new(path.clone(), PathBuf::from("/work/pulse"), None);

        writer.connected().expect("first app-server connection");
        writer
            .observe(&ThreadRegistration {
                thread_id: "other-thread".into(),
                session_id: "other-thread".into(),
                cwd: "/work/other".into(),
            })
            .expect("ignore other cwd");
        writer
            .observe(&ThreadRegistration {
                thread_id: "owned-thread".into(),
                session_id: "root-session".into(),
                cwd: "/work/pulse".into(),
            })
            .expect("record matching thread");
        writer
            .observe(&ThreadRegistration {
                thread_id: "later-thread".into(),
                session_id: "later-thread".into(),
                cwd: "/work/pulse".into(),
            })
            .expect("retain first matching thread");
        writer
            .connected()
            .expect("replacement app-server connection");

        assert_eq!(
            read_launcher_state(&path),
            Some(LauncherState {
                generation: 2,
                connected: true,
                thread_id: Some("owned-thread".into()),
            })
        );
    }

    #[test]
    fn explicit_resume_thread_id_pins_launcher_correlation() {
        let thread_id = "01a04397-805f-73e0-8d8a-b0aada4e0105";
        assert_eq!(
            explicit_resume_thread_id(&[
                "resume".into(),
                "--yolo".into(),
                thread_id.into(),
                "continue the task".into(),
            ]),
            Some(thread_id.into())
        );
        assert_eq!(
            explicit_resume_thread_id(&[
                "resume".into(),
                "--last".into(),
                "01a04397-805f-73e0-8d8a-b0aada4e0105".into(),
            ]),
            None
        );
    }

    #[test]
    fn launcher_state_retains_explicit_thread_over_same_cwd_events() {
        let temp = tempfile::tempdir().expect("launcher state tempdir");
        let path = temp.path().join("state.json");
        let mut writer = LauncherStateWriter::new(
            path.clone(),
            PathBuf::from("/work/pulse"),
            Some("expected-thread".into()),
        );

        writer.connected().expect("app-server connection");
        writer
            .observe(&ThreadRegistration {
                thread_id: "concurrent-thread".into(),
                session_id: "concurrent-thread".into(),
                cwd: "/work/pulse".into(),
            })
            .expect("ignore concurrent thread");

        assert_eq!(
            read_launcher_state(&path).and_then(|state| state.thread_id),
            Some("expected-thread".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_run_falls_back_when_app_server_start_fails() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temporary fake Codex directory");
        let codex = temp.path().join("codex");
        let invocation = temp.path().join("native-args");
        std::fs::write(
            &codex,
            format!(
                "#!/bin/sh\nif [ \"$1\" = app-server ]; then exit 17; fi\nprintf '%s\\n' \"$@\" > '{}'\n",
                invocation.display()
            ),
        )
        .expect("write fake Codex");
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755))
            .expect("make fake Codex executable");

        run_codex_binary(
            codex.to_str().expect("UTF-8 fake Codex path"),
            &["resume".into(), "--last".into(), "--yolo".into()],
        )
        .expect("native fallback succeeds");

        assert_eq!(
            std::fs::read_to_string(invocation).expect("native invocation record"),
            "resume\n--last\n--yolo\n"
        );
    }

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
    fn wake_cooldown_survives_an_empty_unread_snapshot() {
        let session = SessionId(Arc::from("thread"));
        let now = Instant::now();
        let mut cooldowns = HashMap::from([(session.clone(), now + Duration::from_secs(30))]);

        // The inbox has been acknowledged, but the cooldown remains so a
        // related message moments later does not create a second turn.
        retain_active_cooldowns(&mut cooldowns, now + Duration::from_secs(1));
        assert!(cooldowns.contains_key(&session));

        retain_active_cooldowns(&mut cooldowns, now + Duration::from_secs(31));
        assert!(cooldowns.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn unix_bridges_elect_one_wake_leader_and_fail_over() {
        let path = std::env::temp_dir().join(format!(
            "marshal-codex-leader-test-{}.lock",
            uuid::Uuid::new_v4()
        ));
        let now = Instant::now();
        let mut first = WakeLeadership::at(path.clone());
        let mut second = WakeLeadership::at(path.clone());

        assert!(!first.ready(now).expect("first acquires and settles"));
        assert!(!second.ready(now).expect("second observes held lock"));
        assert!(
            first
                .ready(now + WAKE_LEADER_SETTLE)
                .expect("first becomes ready")
        );

        drop(first);
        let takeover = now + WAKE_LEADER_SETTLE + Duration::from_millis(1);
        let acquired_at = (0..100)
            .find_map(|attempt| {
                let poll = takeover + Duration::from_millis(attempt);
                assert!(
                    !second
                        .ready(poll)
                        .expect("second acquires after first exits")
                );
                second.acquired_at.map(|_| poll)
            })
            .expect("second acquires released wake-leader lock");
        assert!(
            second
                .ready(acquired_at + WAKE_LEADER_SETTLE)
                .expect("second becomes ready after settling")
        );

        drop(second);
        let _ = std::fs::remove_file(path);
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

    #[test]
    fn thread_started_uses_root_session_id_and_cwd() {
        let notification = json!({
            "method": "thread/started",
            "params": {
                "thread": {
                    "id": "child-thread",
                    "sessionId": "root-session",
                    "cwd": "/work/pulse"
                }
            }
        });
        assert_eq!(
            thread_registration(&notification),
            Some(ThreadRegistration {
                thread_id: "child-thread".into(),
                session_id: "root-session".into(),
                cwd: "/work/pulse".into(),
            })
        );
        assert!(
            thread_registration(&json!({
                "method": "turn/started",
                "params": {}
            }))
            .is_none()
        );
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

    async fn read_http_request(mut stream: tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut chunk = [0u8; 1024];
        let (header_end, content_length) = loop {
            let n = stream.read(&mut chunk).await.expect("read hook request");
            assert!(n > 0, "hook request closed before headers");
            request.extend_from_slice(&chunk[..n]);
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .expect("content length");
            break (header_end, content_length);
        };
        while request.len() < header_end + 4 + content_length {
            let n = stream.read(&mut chunk).await.expect("read hook body");
            assert!(n > 0, "hook request closed before body");
            request.extend_from_slice(&chunk[..n]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("reply to hook request");
        String::from_utf8(request).expect("UTF-8 hook request")
    }

    #[tokio::test]
    async fn lifecycle_subscriber_is_ready_before_registering_thread_started() {
        use tokio::net::TcpListener as TokioTcpListener;

        let app_listener = TokioTcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake app-server");
        let app_address = app_listener.local_addr().unwrap();
        let hook_listener = TokioTcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake hook listener");
        let hook_address = hook_listener.local_addr().unwrap();
        let ready_file = std::env::temp_dir().join(format!(
            "marshal-codex-registration-test-{}.ready",
            uuid::Uuid::new_v4()
        ));
        let (send_notification, receive_notification) = oneshot::channel::<()>();

        let app_server = tokio::spawn(async move {
            let (stream, _) = app_listener.accept().await.expect("accept bridge");
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

            receive_notification
                .await
                .expect("test releases notification");
            websocket
                .send(WebSocketMessage::Text(
                    json!({
                        "method": "thread/started",
                        "params": {
                            "thread": {
                                "id": "thread-before-prompt",
                                "sessionId": "session-before-prompt",
                                "cwd": "/work/before-prompt"
                            }
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        });
        let hook_server = tokio::spawn(async move {
            let (stream, _) = hook_listener.accept().await.expect("accept hook request");
            read_http_request(stream).await
        });
        let watcher = tokio::spawn(watch_app_server_registrations(
            AppServerEndpoint::WebSocket(format!("ws://{app_address}")),
            format!("http://{hook_address}"),
            Some(ready_file.clone()),
            None,
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            while !ready_file.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("lifecycle subscriber readiness");
        send_notification.send(()).unwrap();

        let request = tokio::time::timeout(Duration::from_secs(2), hook_server)
            .await
            .expect("eager registration request")
            .expect("hook server task");
        assert!(
            request.starts_with("POST /hook/session-register?"),
            "unexpected registration request: {request}"
        );
        let body = request.split_once("\r\n\r\n").unwrap().1;
        let body: Value = serde_json::from_str(body).expect("registration JSON");
        assert_eq!(body["session_id"], "session-before-prompt");
        assert_eq!(body["cwd"], "/work/before-prompt");

        watcher.abort();
        app_server.await.unwrap();
        let _ = fs::remove_file(ready_file);
    }

    #[tokio::test]
    async fn lifecycle_subscriber_registers_thread_started_before_it_connected() {
        use tokio::net::TcpListener as TokioTcpListener;

        let app_listener = TokioTcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake app-server");
        let app_address = app_listener.local_addr().unwrap();
        let hook_listener = TokioTcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake hook listener");
        let hook_address = hook_listener.local_addr().unwrap();

        let app_server = tokio::spawn(async move {
            let (stream, _) = app_listener.accept().await.expect("accept bridge");
            let mut websocket = accept_async(stream).await.expect("WebSocket upgrade");
            let initialize = websocket.next().await.unwrap().unwrap();
            let initialize: Value = serde_json::from_str(initialize.to_text().unwrap()).unwrap();
            websocket
                .send(WebSocketMessage::Text(
                    json!({ "id": initialize["id"], "result": {} })
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let initialized = websocket.next().await.unwrap().unwrap();
            let initialized: Value = serde_json::from_str(initialized.to_text().unwrap()).unwrap();
            assert_eq!(initialized["method"], "initialized");

            let list = websocket.next().await.unwrap().unwrap();
            let list: Value = serde_json::from_str(list.to_text().unwrap()).unwrap();
            assert_eq!(list["method"], "thread/list");
            websocket
                .send(WebSocketMessage::Text(
                    json!({
                        "id": list["id"],
                        "result": {
                            "data": [{
                                "id": "already-running-thread",
                                "status": { "type": "idle" }
                            }],
                            "nextCursor": null
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            let read = websocket.next().await.unwrap().unwrap();
            let read: Value = serde_json::from_str(read.to_text().unwrap()).unwrap();
            assert_eq!(read["method"], "thread/read");
            assert_eq!(read["params"]["threadId"], "already-running-thread");
            websocket
                .send(WebSocketMessage::Text(
                    json!({
                        "id": read["id"],
                        "result": {
                            "thread": {
                                "id": "already-running-thread",
                                "sessionId": "already-running-session",
                                "cwd": "/work/already-running",
                                "status": { "type": "idle" }
                            }
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        });
        let hook_server = tokio::spawn(async move {
            let (stream, _) = hook_listener.accept().await.expect("accept hook request");
            read_http_request(stream).await
        });
        let watcher = tokio::spawn(watch_app_server_registrations(
            AppServerEndpoint::WebSocket(format!("ws://{app_address}")),
            format!("http://{hook_address}"),
            None,
            None,
        ));

        let request = tokio::time::timeout(Duration::from_secs(2), hook_server)
            .await
            .expect("snapshot registration request")
            .expect("hook server task");
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1)
            .expect("registration JSON");
        assert_eq!(body["session_id"], "already-running-session");
        assert_eq!(body["cwd"], "/work/already-running");

        watcher.abort();
        app_server.await.unwrap();
    }

    #[tokio::test]
    async fn tui_proxy_records_connection_scoped_thread_started() {
        let app_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let app_address = app_listener.local_addr().unwrap();
        let reserved = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let proxy_address = reserved.local_addr().unwrap();
        drop(reserved);
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("launcher.json");
        let launcher = Arc::new(tokio::sync::Mutex::new(LauncherStateWriter::new(
            state_path.clone(),
            PathBuf::from("/work/picker"),
            None,
        )));

        let proxy = spawn_tui_proxy(
            Some(AppServerEndpoint::WebSocket(format!(
                "ws://{proxy_address}"
            ))),
            AppServerEndpoint::WebSocket(format!("ws://{app_address}")),
            Some(launcher),
        )
        .await
        .unwrap();
        let app_server = tokio::spawn(async move {
            let (stream, _) = app_listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            websocket
                .send(WebSocketMessage::Text(
                    json!({
                        "method": "thread/started",
                        "params": {
                            "thread": {
                                "id": "picker-thread",
                                "sessionId": "picker-session",
                                "cwd": "/work/picker"
                            }
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        });

        let (mut tui, _) = connect_async(format!("ws://{proxy_address}"))
            .await
            .unwrap();
        let event = tui.next().await.unwrap().unwrap();
        let event: Value = serde_json::from_str(event.to_text().unwrap()).unwrap();
        assert_eq!(event["params"]["thread"]["id"], "picker-thread");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if read_launcher_state(&state_path)
                    .and_then(|state| state.thread_id)
                    .as_deref()
                    == Some("picker-thread")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("proxy should persist picker-selected thread");

        proxy.abort();
        app_server.await.unwrap();
    }

    #[tokio::test]
    async fn tui_proxy_records_picker_thread_resume_request() {
        let app_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let app_address = app_listener.local_addr().unwrap();
        let reserved = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let proxy_address = reserved.local_addr().unwrap();
        drop(reserved);
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("launcher.json");
        let launcher = Arc::new(tokio::sync::Mutex::new(LauncherStateWriter::new(
            state_path.clone(),
            PathBuf::from("/work/picker"),
            None,
        )));

        let proxy = spawn_tui_proxy(
            Some(AppServerEndpoint::WebSocket(format!(
                "ws://{proxy_address}"
            ))),
            AppServerEndpoint::WebSocket(format!("ws://{app_address}")),
            Some(launcher),
        )
        .await
        .unwrap();
        let app_server = tokio::spawn(async move {
            let (stream, _) = app_listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let request = websocket.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "thread/resume");
        });

        let (mut tui, _) = connect_async(format!("ws://{proxy_address}"))
            .await
            .unwrap();
        tui.send(WebSocketMessage::Text(
            json!({
                "id": 7,
                "method": "thread/resume",
                "params": { "threadId": "picker-resumed-thread" }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if read_launcher_state(&state_path)
                    .and_then(|state| state.thread_id)
                    .as_deref()
                    == Some("picker-resumed-thread")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("proxy should persist the picker thread/resume request");

        proxy.abort();
        app_server.await.unwrap();
    }
}
