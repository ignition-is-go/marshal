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
mod mcp;
mod self_update;
mod state_file;
mod statusline;
mod tools;

use anyhow::{Context, Result};
use chrono::Utc;
use hyphae::Watchable;
use marshal_entities::{GetAllSessions, HostInfo, NotifyChannel, Session, SessionId};
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

#[tokio::main]
async fn main() -> Result<()> {
    // Subcommand dispatch. Bare invocation falls through to the MCP
    // server (the default and dominant mode). `--check` is the self-
    // update smoke test. `statusline` is the Claude Code statusLine
    // renderer — folded into this binary so users get one declarative
    // command on every platform.
    let mut argv = std::env::args().skip(1);
    match argv.next().as_deref() {
        Some("--check") if argv.next().is_none() => {
            println!("ok");
            return Ok(());
        }
        Some("statusline") if argv.next().is_none() => {
            return statusline::run();
        }
        Some(other) => {
            anyhow::bail!("unknown argument: {other}");
        }
        None => {}
    }

    init_logging();
    marshal_entities::link();

    let daemon_address = std::env::var(ADDRESS_ENV)
        .or_else(|_| std::env::var(ADDRESS_ENV_LEGACY))
        .unwrap_or_else(|_| DEFAULT_DAEMON_ADDRESS.to_string());

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
    let basename = std::path::Path::new(&cwd)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("session")
        .to_string();
    let git_branch = detect_git_branch(&cwd);
    let project = detect_project_basename(&cwd);
    let operator = detect_operator();
    let host = detect_host();

    // Identity derivation — see `derive_identity` for the rules.
    let (raw_session_id, nickname) = derive_identity(
        |k| std::env::var(k).ok(),
        &basename,
        || Uuid::new_v4().to_string(),
    );
    let session_id = SessionId(Arc::from(raw_session_id));

    let session = Session {
        id: session_id.clone(),
        client_id: None,
        nickname: nickname.clone(),
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
    };
    let session = Arc::new(Mutex::new(session));

    // Write the initial PPID-keyed state file so consumers (e.g. a
    // statusLine script) can resolve the nickname immediately, before
    // the WS handshake completes. The periodic loop below still
    // refreshes this file if a daemon-side rename ever changes our
    // nickname (e.g. an operator using set_nickname).
    state_file::write(&session.lock().unwrap(), &session_id);

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
        session_id: session_id.clone(),
        nickname: nickname.clone(),
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
            "You are session '{nickname}' (id {}) in {cwd}. Coordinate with \
             sibling Claude sessions via the marshal daemon.\n\
             \n\
             READ paths are resources (use `resources/read`):\n\
             - marshal://whoami       — your session id, nickname, pid, cwd, operator, host\n\
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
             Inbound peer messages arrive as `notifications/claude/channel` \
             events; reply with `send_message` or `broadcast`.",
            session_id.0
        ),
        tools: tools::tools_def(),
        resources: tools::resources_def(),
    };

    // Activity tracker: bumped by the MCP dispatcher on each request and
    // start/end-bracketed around tools/call. The self-update watcher uses
    // it to find a safe moment to re-exec; the roster-publish loop uses
    // it to keep `Session.last_activity_at` / `last_tool` / `last_tool_at`
    // current upstream.
    let activity = Arc::new(activity::Activity::new());
    self_update::spawn(Arc::clone(&activity));

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
    let session_id_for_publish = session_id.clone();
    let sessions_cell_for_publish = handler.host.sessions_cell.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Cache the last-pushed values so we only dispatch setters
        // that actually changed — keeps the daemon's event log from
        // accumulating no-op SETs every tick.
        let mut pushed_activity_at: Option<i64> = None;
        let mut pushed_tool: Option<String> = None;
        let mut pushed_tool_at: Option<i64> = None;
        let mut last_state_nickname: Option<String> = None;
        loop {
            interval.tick().await;

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

            // Refresh the PPID-keyed state file with the *daemon-side*
            // nickname (post-dedupe). Look ourselves up in the live
            // roster rather than trusting the local mirror, which only
            // ever holds the un-dedup'd basename we sent at startup.
            let snapshot: Vec<Arc<marshal_entities::Session>> =
                hyphae::Gettable::get(&sessions_cell_for_publish);
            if let Some(me) = snapshot.iter().find(|s| s.id == session_id_for_publish) {
                if last_state_nickname.as_deref() != Some(me.nickname.as_str()) {
                    state_file::write(me, &session_id_for_publish);
                    last_state_nickname = Some(me.nickname.clone());
                }
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

fn init_logging() {
    let mut b = env_logger::Builder::from_default_env();
    if std::env::var("RUST_LOG").is_err() {
        b.filter_level(log::LevelFilter::Info);
    }
    b.target(env_logger::Target::Stderr).init();
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

/// Build the marshal `Session.id` + display nickname from process state.
///
/// `CLAUDE_CODE_SESSION_ID` (Claude Code's conversation UUID, set on
/// every MCP subprocess) is preferred when present so identity survives
/// `claude --resume`: the same Session.id binds the persisted Session
/// entity across shim restarts within one conversation. The eight-char
/// prefix gets stitched onto the nickname (`pulse-deploy@a1836feb`) so
/// two concurrent claude instances in the same cwd produce structurally
/// distinct names — no daemon-side dedupe pass needed in the common
/// case.
///
/// Fallback: a fresh UUID for non-Claude-Code drivers (the TUI in
/// observer mode, future MCP clients, ad-hoc shells). The dedupe saga
/// still exists as a safety net for that path.
///
/// `env` and `uuid` are injected as closures so the function is unit-
/// testable without poking at process env.
fn derive_identity(
    env: impl Fn(&str) -> Option<String>,
    basename: &str,
    uuid: impl Fn() -> String,
) -> (String, String) {
    let raw = env("CLAUDE_CODE_SESSION_ID")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(uuid);
    let short: String = raw.chars().take(8).collect();
    let nickname = format!("{basename}@{short}");
    (raw, nickname)
}

#[cfg(test)]
mod identity_tests {
    use super::derive_identity;

    #[test]
    fn uses_claude_code_session_id_when_set() {
        let (id, nick) = derive_identity(
            |k| {
                (k == "CLAUDE_CODE_SESSION_ID")
                    .then(|| "a1836feb-6e6b-43f1-949e-d2fb10d1bfa5".to_string())
            },
            "pulse-deploy",
            || panic!("uuid generator should not be called when env var is set"),
        );
        assert_eq!(id, "a1836feb-6e6b-43f1-949e-d2fb10d1bfa5");
        assert_eq!(nick, "pulse-deploy@a1836feb");
    }

    #[test]
    fn falls_back_to_uuid_when_env_unset() {
        let (id, nick) = derive_identity(
            |_| None,
            "marshal",
            || "deadbeef-0000-0000-0000-000000000000".to_string(),
        );
        assert_eq!(id, "deadbeef-0000-0000-0000-000000000000");
        assert_eq!(nick, "marshal@deadbeef");
    }

    #[test]
    fn empty_env_value_treated_as_unset() {
        // CLAUDE_CODE_SESSION_ID="" is "set but empty"; treat the same
        // as unset so we don't end up with a nickname like `foo@`.
        let (id, nick) = derive_identity(
            |k| (k == "CLAUDE_CODE_SESSION_ID").then(String::new),
            "foo",
            || "11111111-2222-3333-4444-555555555555".to_string(),
        );
        assert_eq!(id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(nick, "foo@11111111");
    }

    #[test]
    fn two_claude_instances_same_cwd_get_distinct_nicknames() {
        // The structural-uniqueness claim: same basename, different
        // session UUIDs from two concurrent claude processes → distinct
        // nicknames without any dedupe machinery.
        let (_, nick_a) = derive_identity(
            |_| Some("aaaaaaaa-1111-1111-1111-111111111111".to_string()),
            "pulse-deploy",
            || unreachable!(),
        );
        let (_, nick_b) = derive_identity(
            |_| Some("bbbbbbbb-2222-2222-2222-222222222222".to_string()),
            "pulse-deploy",
            || unreachable!(),
        );
        assert_ne!(nick_a, nick_b);
        assert_eq!(nick_a, "pulse-deploy@aaaaaaaa");
        assert_eq!(nick_b, "pulse-deploy@bbbbbbbb");
    }

    #[test]
    fn shorter_than_eight_char_id_does_not_panic() {
        // Hedge against a future Claude Code identity format change.
        let (id, nick) = derive_identity(
            |_| Some("abcd".to_string()),
            "foo",
            || unreachable!(),
        );
        assert_eq!(id, "abcd");
        assert_eq!(nick, "foo@abcd");
    }
}
