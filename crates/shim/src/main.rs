//! marshal-shim — stdio MCP server backed by a MykoClient.
//!
//! On startup the shim:
//! 1. connects MykoClient to MARSHAL_DAEMON_ADDRESS (default
//!    ws://localhost:6155),
//! 2. SETs a `Session` entity describing this Claude session,
//! 3. registers `on_command::<NotifyChannel>` so daemon-pushed notifications
//!    (currently: peer messages via `MessageNotifySaga`) are forwarded as
//!    `notifications/claude/channel` MCP events,
//! 4. serves stdio MCP with a curated tool surface backed by the MykoClient.

mod activity;
mod channels;
mod mcp;
mod session_discovery;
mod statusline;
mod tools;

use anyhow::{Context, Result};
use chrono::Utc;
use hyphae::Watchable;
use marshal_entities::{GetAllSessions, HostInfo, NotifyChannel, Session};
use mcp::ServerConfig;
use myko::{
    client::{ConnectionStatus, MykoClient},
    wire::{MEvent, MEventType},
};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use uuid::Uuid;

const DEFAULT_DAEMON_ADDRESS: &str = "ws://localhost:6155";

/// Env var that overrides the daemon WebSocket URL. Set this to point
/// the shim at a daemon other than the default `ws://localhost:6155` —
/// e.g. a daemon on another host or a non-default port. The plugin's
/// `.mcp.json` plumbs this through with `${MARSHAL_DAEMON_ADDRESS}`
/// substitution, so a user-shell `export` is enough to reach Claude
/// Code's spawned shim. The legacy `MYKO_ADDRESS` name is honored as
/// a fallback so existing setups don't break on upgrade.
const ADDRESS_ENV: &str = "MARSHAL_DAEMON_ADDRESS";
const ADDRESS_ENV_LEGACY: &str = "MYKO_ADDRESS";

/// Filename the shim reads from a per-user config dir when neither
/// `MARSHAL_DAEMON_ADDRESS` nor `MYKO_ADDRESS` is set in the
/// environment. The file contains a single line: the daemon URL.
///
/// Why: env-var propagation across shells (VS Code terminal, dev-channels
/// plugin spawn, Git Bash, cmd.exe) is fragile — a user-level env var set
/// after a parent process started isn't seen by that process or its
/// children. A file at a fixed path the operator owns is read fresh on
/// every shim startup, so it works regardless of how the shim was
/// invoked.
///
/// Search order, first match wins:
/// 1. Linux/macOS: `$XDG_CONFIG_HOME/marshal/daemon-address`, then
///    `$HOME/.config/marshal/daemon-address`.
/// 2. Windows: `%APPDATA%\marshal\daemon-address`, then
///    `%PROGRAMDATA%\marshal\daemon-address`.
const ADDRESS_FILE: &str = "daemon-address";

fn main() -> Result<()> {
    // Subcommands are dispatched BEFORE the async runtime is built so the
    // statusline (invoked on every Claude render) and the deploy smoke
    // test never pay for tokio / WS / MCP init. This is why both live as
    // subcommands of the one binary instead of separate artifacts that
    // would have to be built, deployed, and kept in lockstep.
    //
    // `--check` is the deploy role's idempotency smoke test ("does the
    // installed binary run on this host"). `statusline` renders Claude
    // Code's status prefix from stdin.
    let mut argv = std::env::args().skip(1);
    match argv.next().as_deref() {
        Some("--check") if argv.next().is_none() => {
            println!("ok");
            return Ok(());
        }
        Some("statusline") if argv.next().is_none() => {
            statusline::render();
            return Ok(());
        }
        Some(other) => {
            anyhow::bail!("unknown argument: {other}");
        }
        None => {}
    }

    // Only the MCP-server path needs async. `#[tokio::main]` defaults to a
    // multi-thread runtime with all features; build the same explicitly so
    // the subcommands above stay runtime-free.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?
        .block_on(serve())
}

async fn serve() -> Result<()> {
    init_logging();
    marshal_entities::link();

    // Resolution order, first match wins:
    //   1. config file at the per-user well-known path
    //   2. MARSHAL_DAEMON_ADDRESS env var
    //   3. legacy MYKO_ADDRESS env var
    //   4. compiled-in localhost default
    //
    // Config file BEFORE env (inverted from the usual env-wins convention)
    // because the shim's parent process tree is brittle on Windows: a
    // VS Code instance launched before fleet config was finalized
    // captures stale user-env values at start, and every terminal (and
    // every Claude.exe, and every shim) downstream of it inherits those
    // stale values. Claude Code's `.claude.json` mcpServers env block
    // does not reliably override inherited env in its stdio MCP spawn,
    // so env-wins meant a leaked value from a stale parent shadowed the
    // role-deployed per-host config file. Config-wins makes the file the
    // per-host source of truth; env becomes a deliberate one-off
    // override the operator can apply by deleting the file.
    let daemon_address = read_address_from_config_file()
        .or_else(|| std::env::var(ADDRESS_ENV).ok())
        .or_else(|| std::env::var(ADDRESS_ENV_LEGACY).ok())
        .unwrap_or_else(|| DEFAULT_DAEMON_ADDRESS.to_string());

    log::info!("[marshal-shim] connecting to {daemon_address}");

    let client = Arc::new(MykoClient::new());

    // Register on_command::<NotifyChannel> *before* we connect, so daemon-
    // pushed notifications that arrive between Session-SET and MCP-init are
    // buffered into a channel rather than dropped. The drain task that
    // forwards buffered notifications onto stdout is spawned later, once
    // the MCP `Notifier` exists.
    let (notify_tx, notify_rx) = mpsc::unbounded_channel::<NotifyChannel>();
    let notify_tx_oncmd = notify_tx.clone();
    let notify_guard = client.on_command::<NotifyChannel, _>(move |cmd, _responder| {
        let _ = notify_tx_oncmd.send(cmd);
    });
    Box::leak(Box::new(notify_guard));

    // Local session metadata.
    let cwd = std::env::current_dir()
        .context("getting cwd")?
        .display()
        .to_string();
    let pid = std::process::id();
    let git_branch = detect_git_branch(&cwd);
    let project = detect_project_basename(&cwd);
    let operator = detect_operator();
    let host = detect_host();

    // Claude Code's canonical session_id is the filename of its per-session
    // transcript at `~/.claude/projects/<encoded_cwd>/<id>.jsonl`. Adopting
    // that id keeps shim + hook on a single Session row keyed by it. We
    // hard-fail if discovery doesn't converge — better to die loudly than
    // register under a synthetic id and silently break peer routing.
    let Some(session_id) = session_discovery::resolve(&cwd) else {
        anyhow::bail!(
            "could not discover Claude Code session_id from ~/.claude/projects/*/*.jsonl \
             (cwd={cwd}); refusing to start under a synthetic id"
        );
    };

    // Resolve whether this session can actually RECEIVE live peer messages —
    // i.e. whether claude was launched with the channels flag. There is no
    // in-band MCP signal for this, so it's read from the parent cmdline
    // (blocking; Windows shells out to Get-CimInstance once). Reported to the
    // daemon on the Session so SendMessage can report delivered_live HONESTLY
    // (a flag-off recipient is queued to its inbox, never claimed as live).
    // `None` = parent unreadable (legacy/unknown). The user-facing RECV-OFF
    // warning does NOT depend on this startup read — the statusline detects
    // live on every render, which is race-free across resume.
    let channels_enabled = tokio::task::spawn_blocking(channels::detect)
        .await
        .unwrap_or(None);

    let session = Session {
        id: session_id.clone(),
        client_id: None,
        pid,
        cwd: cwd.clone(),
        git_branch: git_branch.clone(),
        current_task: None,
        connected_at: Utc::now().timestamp_millis(),
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
        operator: Some(operator.clone()),
        host: Some(host.clone()),
        project: project.clone(),
        channels_enabled,
    };
    let session = Arc::new(Mutex::new(session));

    // Open the long-lived `GetAll*` subscriptions BEFORE we connect so
    // they're primed by the time the WS handshake completes — tools and
    // resources that snapshot them (roster, rooms, send_message
    // recipient resolution) don't race the server's first response.
    let sessions_cell = client.watch_query::<GetAllSessions>(GetAllSessions {});
    let rooms_cell =
        client.watch_query::<marshal_entities::GetAllRooms>(marshal_entities::GetAllRooms {});
    let members_cell = client
        .watch_query::<marshal_entities::GetAllRoomMembers>(marshal_entities::GetAllRoomMembers {});

    // Re-SET our Session on every connect. The daemon holds session state
    // in-memory, so a daemon restart drops every roster entry; we have to
    // re-publish on reconnect or peers can't see us anymore. This also
    // handles the initial connection — the subscriber fires synchronously
    // the moment the WebSocket opens.
    let session_for_resend = Arc::clone(&session);
    let client_for_resend = Arc::clone(&client);
    let conn_guard = client.connection_status().subscribe(move |signal| {
        if let hyphae::Signal::Value(status) = signal {
            match &**status {
                ConnectionStatus::Connected(addr) => {
                    log::info!("[marshal-shim] connected to {addr} — (re)sending session");
                    let snapshot = session_for_resend.lock().unwrap().clone();
                    if let Err(e) = emit_session_set(&client_for_resend, &snapshot) {
                        log::warn!("[marshal-shim] re-SET on connect failed: {e}");
                    }
                }
                ConnectionStatus::Disconnected => {
                    log::warn!("[marshal-shim] disconnected");
                }
                _ => {}
            }
        }
    });
    client.connection_status().own(conn_guard);

    // All queries / handlers / connection subscribers are registered.
    // Now it's safe to start the WS handshake.
    client.set_address(Some(daemon_address));

    let host = Arc::new(tools::ToolHost {
        client: Arc::clone(&client),
        pid,
        cwd: cwd.clone(),
        session: Arc::clone(&session),
        sessions_cell,
        rooms_cell,
        members_cell,
    });

    let handler = Arc::new(tools::CoordHandler { host });

    let config = ServerConfig {
        name: "marshal-shim".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        instructions: format!(
            "You are marshal session {} in {cwd}. Coordinate with sibling \
             Claude sessions via the marshal daemon.\n\
             \n\
             READ paths are resources (use `resources/read`):\n\
             - marshal://whoami       — your session id, pid, cwd, operator, host\n\
             - marshal://roster       — every live session and what room(s) it's in\n\
             - marshal://rooms        — every room and who its members are\n\
             - marshal://messages     — message history; supports query params:\n\
                                       inbox=true, sent=true, unread=true,\n\
                                       room=ID, from=SID, to_session=SID,\n\
                                       since=MILLIS, limit=N\n\
             \n\
             WRITE paths are tools (use `tools/call`):\n\
             - send_message       — direct send to a peer's session_id\n\
             - broadcast          — fan-out to all members of a room\n\
             - join_room          — create or join an ad-hoc room\n\
             - leave_room         — leave an ad-hoc room\n\
             - set_status         — set this session's free-form status text\n\
             - ack_messages       — mark message ids as read for this session\n\
             \n\
             Each session has a memorable `nickname` (e.g. `swift-falcon`) — a \
             deterministic adjective-noun derived from its session_id, shown in \
             its statusline and in marshal://roster. Address peers by nickname, \
             session_id, or session_id prefix (send_message resolves any of \
             them). The `swift-falcon` after a peer's path is its nickname, NOT \
             a git branch or commit.\n\
             \n\
             Inbound peer messages arrive as `notifications/claude/channel` \
             events; reply with `send_message` or `broadcast`.",
            session_id.0
        ),
        tools: tools::tools_def(),
        resources: tools::resources_def(),
    };

    // Event-driven git-branch refresh. A checkout/switch rewrites the repo's
    // `.git/HEAD`; an OS fs-watch on it pushes the new branch to the roster the
    // moment it changes — no polling, no per-tick git spawn. The operator's own
    // statusline already recomputes the branch per render; this keeps PEERS'
    // roster view current too. The debouncer is leaked to live for the process
    // lifetime; the callback reads the live session id (so it follows an id
    // reconcile) and only pushes when the branch actually changed.
    if let Some(head_dir) = resolve_git_head_dir(&cwd) {
        use notify_debouncer_mini::{DebounceEventResult, new_debouncer, notify::RecursiveMode};
        let client_for_watch = Arc::clone(&client);
        let session_for_watch = Arc::clone(&session);
        let cwd_for_watch = cwd.clone();
        let mut last_branch = git_branch.clone(); // startup value is already SET
        match new_debouncer(
            std::time::Duration::from_millis(200),
            move |result: DebounceEventResult| {
                let touched_head = matches!(&result, Ok(events)
                    if events.iter().any(|e| e.path.file_name().is_some_and(|n| n == "HEAD")));
                if !touched_head {
                    return;
                }
                let current = detect_git_branch(&cwd_for_watch);
                if current == last_branch {
                    return;
                }
                last_branch = current.clone();
                let id = session_for_watch.lock().unwrap().id.clone();
                let arc_branch = current.as_deref().map(Arc::<str>::from);
                let _ = client_for_watch.send_command::<marshal_entities::SetSessionGitBranch, ()>(
                    &marshal_entities::SetSessionGitBranch {
                        id,
                        git_branch: arc_branch,
                    },
                );
                if let Ok(mut s) = session_for_watch.lock() {
                    s.git_branch = current;
                }
                log::info!(
                    "[marshal-shim] git branch changed -> {last_branch:?}; pushed to roster"
                );
            },
        ) {
            Ok(mut debouncer) => match debouncer
                .watcher()
                .watch(&head_dir, RecursiveMode::NonRecursive)
            {
                Ok(()) => {
                    log::info!(
                        "[marshal-shim] watching {} for branch changes",
                        head_dir.display()
                    );
                    Box::leak(Box::new(debouncer));
                }
                Err(e) => log::warn!("[marshal-shim] HEAD watch failed: {e}"),
            },
            Err(e) => log::warn!("[marshal-shim] git watcher init failed: {e}"),
        }
    }

    // Activity tracker: bumped by the MCP dispatcher on each request and
    // start/end-bracketed around tools/call. The roster-publish loop uses
    // it to keep `Session.last_activity_at` / `last_tool` / `last_tool_at`
    // current upstream.
    let activity = Arc::new(activity::Activity::new());

    // Roster liveness publisher: every 5s, dispatch the three liveness
    // setters with the current snapshot. The cadence is a deliberate
    // compromise — per-tool-call would flood the daemon, while a
    // longer interval would lag the staleness-detection signal.
    // Setter dispatch is cheap (single command, server-side write) so
    // 5s × ~12/min × ~hours of session is negligible.
    //
    // The publisher also keeps the local `session` mirror in sync so
    // a reconnect re-SET (which sends the full Session entity by
    // design) reflects the latest liveness values rather than
    // clobbering them with stale defaults.
    let activity_for_publish = Arc::clone(&activity);
    let client_for_publish = Arc::clone(&client);
    let session_for_publish = Arc::clone(&session);
    // The parent `claude` pid is stable for this shim's lifetime; we poll its
    // live per-PID manifest to detect canonical-id drift (compact/clear).
    let parent_pid_for_publish = session_discovery::parent_pid();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Cache the last-pushed values so we only dispatch setters
        // that actually changed — keeps the daemon's event log from
        // accumulating no-op SETs every tick.
        let mut pushed_activity_at: Option<i64> = None;
        let mut pushed_tool: Option<String> = None;
        let mut pushed_tool_at: Option<i64> = None;
        loop {
            interval.tick().await;

            // Self-heal canonical-id drift. On a compact/clear, Claude re-mints
            // this session's id but keeps the same process + MCP shim running,
            // so our inherited CLAUDE_CODE_SESSION_ID env is frozen at the OLD
            // id. Claude DOES keep its per-PID manifest current, so we poll it:
            // if the canonical id moved, DEL the stale Session and re-SET under
            // the new id. The re-SET also refreshes the client_id binding from
            // the live connection — which is what unblocks send_message without
            // a manual `/mcp reconnect`.
            if let Some(current_canonical) =
                parent_pid_for_publish.and_then(session_discovery::canonical_session_id)
            {
                let registered = session_for_publish.lock().unwrap().id.clone();
                if current_canonical != registered {
                    log::warn!(
                        "[marshal-shim] canonical session id drifted {} -> {}; re-registering",
                        registered.0,
                        current_canonical.0
                    );
                    let stale = session_for_publish.lock().unwrap().clone();
                    if let Err(e) = emit_session_del(&client_for_publish, &stale) {
                        log::warn!("[marshal-shim] DEL of stale session failed: {e}");
                    }
                    let refreshed = {
                        let mut sess = session_for_publish.lock().unwrap();
                        sess.id = current_canonical.clone();
                        sess.clone()
                    };
                    if let Err(e) = emit_session_set(&client_for_publish, &refreshed) {
                        log::warn!("[marshal-shim] re-SET under new id failed: {e}");
                    }
                }
            }

            // Target the CURRENT id (it may have just been reconciled above) so
            // liveness setters never write to a stale/dead session row.
            let session_id_for_publish = session_for_publish.lock().unwrap().id.clone();

            let last_activity_at = activity_for_publish.last_activity_ms();
            let last_activity_at = if last_activity_at > 0 {
                Some(last_activity_at)
            } else {
                None
            };
            let last_tool = activity_for_publish.last_tool_name();
            let last_tool_at = activity_for_publish.last_tool_ms();
            let last_tool_at = if last_tool_at > 0 {
                Some(last_tool_at)
            } else {
                None
            };

            if pushed_activity_at != last_activity_at {
                let _ = client_for_publish
                    .send_command::<marshal_entities::SetSessionLastActivityAt, ()>(
                        &marshal_entities::SetSessionLastActivityAt {
                            id: session_id_for_publish.clone(),
                            last_activity_at,
                        },
                    );
                pushed_activity_at = last_activity_at;
            }
            if pushed_tool != last_tool {
                let arc_tool = last_tool.as_deref().map(Arc::<str>::from);
                let _ = client_for_publish
                    .send_command::<marshal_entities::SetSessionLastTool, ()>(
                        &marshal_entities::SetSessionLastTool {
                            id: session_id_for_publish.clone(),
                            last_tool: arc_tool,
                        },
                    );
                pushed_tool = last_tool.clone();
            }
            if pushed_tool_at != last_tool_at {
                let _ = client_for_publish
                    .send_command::<marshal_entities::SetSessionLastToolAt, ()>(
                        &marshal_entities::SetSessionLastToolAt {
                            id: session_id_for_publish.clone(),
                            last_tool_at,
                        },
                    );
                pushed_tool_at = last_tool_at;
            }

            // Mirror to the local Session so reconnect re-SETs
            // include the latest liveness values.
            if let Ok(mut sess) = session_for_publish.lock() {
                sess.last_activity_at = last_activity_at;
                sess.last_tool = last_tool.clone();
                sess.last_tool_at = last_tool_at;
            }
        }
    });

    let notify_rx = Mutex::new(Some(notify_rx));
    mcp::serve_stdio(config, handler, Arc::clone(&activity), move |notifier| {
        // Spawn a task that drains the NotifyChannel buffer and emits each
        // one onto stdout via the MCP writer. The buffer accumulated any
        // notifications that fired before MCP init.
        if let Some(mut rx) = notify_rx.lock().ok().and_then(|mut g| g.take()) {
            tokio::spawn(async move {
                while let Some(cmd) = rx.recv().await {
                    notifier.channel(cmd.content, cmd.meta);
                }
            });
            log::info!("[marshal-shim] notification drain task started");
        }
    })
    .await
}

/// SET our Session entity. Used both on initial connect and on every
/// subsequent reconnect (the daemon's in-memory store loses everything
/// when it restarts, so we have to re-publish or peers can't see us).
/// The server auto-populates `client_id` from the WS connection.
fn emit_session_set(client: &MykoClient, session: &Session) -> Result<()> {
    let event = MEvent::from_item(session, MEventType::SET, &Uuid::new_v4().to_string());
    client
        .send_event(event)
        .map_err(|e| anyhow::anyhow!("send_event failed: {e}"))?;
    Ok(())
}

/// DEL a Session entity. Used by the canonical-id-drift reconcile to remove the
/// stale roster row after the session id changes under us (compact/clear), so
/// peers don't see a ghost under the old id. The daemon's cleanup sweep is a
/// backstop if this DEL is lost.
fn emit_session_del(client: &MykoClient, session: &Session) -> Result<()> {
    let event = MEvent::from_item(session, MEventType::DEL, &Uuid::new_v4().to_string());
    client
        .send_event(event)
        .map_err(|e| anyhow::anyhow!("send_event(DEL) failed: {e}"))?;
    Ok(())
}

/// Try each well-known per-user config path in order; return the first
/// non-empty trimmed line from the first readable file. Trailing newlines
/// and surrounding whitespace are stripped so an operator can `echo URL >
/// daemon-address` without worrying about formatting.
fn read_address_from_config_file() -> Option<String> {
    for path in address_file_candidates() {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let line = contents.lines().next().unwrap_or("").trim();
            if !line.is_empty() {
                log::info!(
                    "[marshal-shim] daemon address from config file {}: {}",
                    path.display(),
                    line
                );
                return Some(line.to_string());
            }
        }
    }
    None
}

#[cfg(unix)]
fn address_file_candidates() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        out.push(
            std::path::PathBuf::from(xdg)
                .join("marshal")
                .join(ADDRESS_FILE),
        );
    }
    if let Some(home) = std::env::var_os("HOME") {
        out.push(
            std::path::PathBuf::from(home)
                .join(".config")
                .join("marshal")
                .join(ADDRESS_FILE),
        );
    }
    out
}

#[cfg(windows)]
fn address_file_candidates() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        out.push(
            std::path::PathBuf::from(appdata)
                .join("marshal")
                .join(ADDRESS_FILE),
        );
    }
    if let Some(pd) = std::env::var_os("PROGRAMDATA") {
        out.push(
            std::path::PathBuf::from(pd)
                .join("marshal")
                .join(ADDRESS_FILE),
        );
    }
    out
}

#[cfg(not(any(unix, windows)))]
fn address_file_candidates() -> Vec<std::path::PathBuf> {
    Vec::new()
}

fn init_logging() {
    let mut b = env_logger::Builder::from_default_env();
    if std::env::var("RUST_LOG").is_err() {
        b.filter_level(log::LevelFilter::Info);
    }
    b.target(env_logger::Target::Stderr).init();
}

/// Directory containing the repo's `HEAD` file for `cwd`: `<cwd>/.git` for a
/// normal checkout, or the resolved `gitdir:` target when `.git` is a file
/// (git worktree / submodule). `None` when `cwd` isn't a git repo. The HEAD
/// watcher watches this directory (non-recursive) and filters for `HEAD`.
fn resolve_git_head_dir(cwd: &str) -> Option<std::path::PathBuf> {
    let dot_git = std::path::Path::new(cwd).join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    // `.git` is a file: `gitdir: <path>` (absolute, or relative to cwd).
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let target = contents.trim().strip_prefix("gitdir:")?.trim();
    let gitdir = std::path::Path::new(target);
    Some(if gitdir.is_absolute() {
        gitdir.to_path_buf()
    } else {
        std::path::Path::new(cwd).join(gitdir)
    })
}

fn detect_git_branch(cwd: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() || s == "HEAD" {
        None
    } else {
        Some(s.to_string())
    }
}

/// Resolve the project name for this session — the basename of the
/// git repo root containing `cwd`. `None` when `cwd` isn't inside a
/// git repo. Anchors the daemon's `project:<basename>` auto-room.
fn detect_project_basename(cwd: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let toplevel = String::from_utf8(out.stdout).ok()?;
    let toplevel = toplevel.trim();
    std::path::Path::new(toplevel)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Resolve which human this session belongs to. `MARSHAL_OPERATOR`
/// wins (the explicit override for service users / shared boxes),
/// then `$USER` (cross-platform unix), then `$USERNAME` (Windows
/// fallback), then `"anonymous"` so we never fail to set an operator
/// at all.
fn detect_operator() -> String {
    std::env::var("MARSHAL_OPERATOR")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "anonymous".to_string())
}

/// Build a `HostInfo` from `gethostname` + `std::env::consts`. Hostname
/// falls back to `"unknown"` when the OS lookup fails (rare — usually
/// only inside heavily restricted sandboxes).
fn detect_host() -> HostInfo {
    let name = gethostname::gethostname()
        .into_string()
        .unwrap_or_else(|_| "unknown".to_string());
    HostInfo {
        name,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{detect_git_branch, resolve_git_head_dir};
    use std::process::Command;

    fn git(args: &[&str], cwd: &std::path::Path) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("run git")
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// The branch the roster shows must track the ACTUAL repo HEAD — this
    /// exercises the real `git`/`.git` reading the HEAD watcher feeds from
    /// (the watcher's trigger is an OS fs-event; the value it pushes comes
    /// from exactly this path), against a real repo across a real checkout.
    #[test]
    fn detect_git_branch_follows_a_real_checkout() {
        let tmp =
            std::env::temp_dir().join(format!("marshal-shim-gitbranch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let cwd = tmp.to_str().unwrap();

        git(&["init", "-q"], &tmp);
        git(&["config", "user.email", "t@t"], &tmp);
        git(&["config", "user.name", "t"], &tmp);
        git(&["commit", "-q", "--allow-empty", "-m", "init"], &tmp);
        git(&["checkout", "-q", "-b", "feature/x"], &tmp);

        // `.git` is a real directory here → HEAD dir is `<cwd>/.git`.
        assert_eq!(resolve_git_head_dir(cwd), Some(tmp.join(".git")));
        assert_eq!(detect_git_branch(cwd).as_deref(), Some("feature/x"));

        // Switch branches — the read must follow HEAD, not stay frozen.
        git(&["checkout", "-q", "-b", "other"], &tmp);
        assert_eq!(detect_git_branch(cwd).as_deref(), Some("other"));

        // Not a repo → no HEAD dir, no branch.
        let bare = tmp.join("nope");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(resolve_git_head_dir(bare.to_str().unwrap()), None);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
