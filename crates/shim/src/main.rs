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
mod codex_bridge;
mod codex_hook;
mod codex_setup;
mod mcp;
mod session_discovery;
mod statusline;
mod tools;

use anyhow::{Context, Result};
use chrono::Utc;
use hyphae::{Gettable, Watchable};
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

/// Publisher ticks (5s each) our own session may be absent from the roster before
/// we conclude the registration was lost and re-SET. Two ticks (~10s) is long
/// enough that a just-sent SET has propagated, short enough that a dropped one
/// heals quickly instead of leaving the session write-blocked indefinitely.
const ROSTER_MISS_TICKS: u32 = 2;

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
        // Codex hook bridge (cross-platform, no shell): Codex runs
        // `marshal-shim codex-hook session-start|prompt-submit [base-url]` from
        // its `[hooks]` config. Also runs runtime-free — a single blocking POST
        // to the daemon's /hook/* endpoint. See `codex_hook`.
        Some("codex-hook") => {
            let ep = argv.next().unwrap_or_else(|| "prompt-submit".to_string());
            let base = argv.next();
            codex_hook::run(&ep, base.as_deref());
            return Ok(());
        }
        // One-shot cross-platform setup: wire marshal into a Codex install
        // (CLI / IDE / desktop app) by writing the config.toml + AGENTS.md
        // managed blocks. For laptops / the desktop app where there's no
        // Ansible. See `codex_setup`.
        Some("codex-setup") => {
            let rest: Vec<String> = argv.collect();
            codex_setup::run(&rest)?;
            return Ok(());
        }
        // Opt-in Codex launcher with real idle-session wakeups. It starts the
        // managed Codex app-server, runs the local marshal bridge, then attaches
        // the normal Codex TUI to that shared server. See `codex_bridge`.
        Some("codex-run") => {
            let rest: Vec<String> = argv.collect();
            codex_bridge::run_codex(&rest)?;
            return Ok(());
        }
        // Long-lived local half of Codex live delivery. Normally started by
        // `codex-run`; exposed as a subcommand so services and tests can run it
        // independently.
        Some("codex-bridge") => {
            let rest: Vec<String> = argv.collect();
            return tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?
                .block_on(codex_bridge::run(&rest));
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
    // Capture the spawner's pid FIRST — before any other startup work — so
    // the parent-death watchdog below baselines against the real spawner.
    // Captured later, a spawner that died during our own startup would have
    // already reparented us and the watchdog would baseline against the
    // subreaper instead, never firing.
    #[cfg(unix)]
    let initial_ppid = unsafe { libc::getppid() };

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
    let operator = detect_operator(&cwd);
    let host = detect_host();

    // Harness selection. Claude Code (default) discovers its canonical
    // session_id from `~/.claude` transcripts; Codex from its `~/.codex`
    // rollout store — Codex hands the id to neither its MCP servers nor this
    // shim (openai/codex#19937), so both harnesses learn it out-of-band from
    // disk. Set `MARSHAL_HARNESS=codex` in the `[mcp_servers.marshal].env`
    // block. Adopting the same id the daemon's `/hook/*` endpoints receive
    // keeps shim + hooks on a single Session row; we hard-fail rather than
    // register under a synthetic id and silently break peer routing.
    let is_codex = std::env::var("MARSHAL_HARNESS")
        .map(|h| h.eq_ignore_ascii_case("codex"))
        .unwrap_or(false);

    let (session_id, git_branch) = if is_codex {
        // Codex harness: the shim does NOT own identity. Codex never tells its
        // MCP servers which session they serve (openai/codex#19937), and disk
        // discovery is unreliable (spawn-vs-write race, resume, same-cwd
        // concurrency) — inferring an id gets peer attribution wrong, which
        // breaks reply routing. So under Codex the shim registers NO Session
        // and never sends with an inferred id: the daemon's SessionStart hook
        // registers the authoritative Session (Codex hands the hook the real
        // id) and injects it, and the agent passes it back as `asSession` on
        // write tools, which the shim forwards. This id is a local placeholder
        // that is never published to the daemon (registration is skipped
        // below), present only to satisfy the shared `ToolHost` shape.
        (
            SessionId(std::sync::Arc::from(format!("codex-shim-{pid}"))),
            git_branch,
        )
    } else {
        match session_discovery::resolve(&cwd) {
            Some(sid) => (sid, git_branch),
            None => anyhow::bail!(
                "could not discover Claude Code session_id from ~/.claude/projects/*/*.jsonl \
                 (cwd={cwd}); refusing to start under a synthetic id"
            ),
        }
    };

    // Resolve whether this session can actually RECEIVE live peer messages —
    // i.e. whether claude was launched with the channels flag. There is no
    // in-band MCP signal for this, so it's read from the parent cmdline
    // (blocking; Windows shells out to Get-CimInstance once). Reported to the
    // daemon on the Session so SendMessage can report delivered_live HONESTLY
    // (a flag-off recipient is queued to its inbox, never claimed as live).
    // `None` = parent unreadable (legacy/unknown). The user-facing RECV-OFF
    // warning does NOT depend on this startup read — the statusline detects
    // live on every render, which is race-free across resume. Codex has no
    // channels flag and delivers inbound via hooks, so it's simply unknown.
    let channels_enabled = if is_codex {
        None
    } else {
        tokio::task::spawn_blocking(channels::detect)
            .await
            .unwrap_or(None)
    };

    // Live fields from Claude's per-PID manifest: the session name (its
    // `/rename` value or auto-title), the busy/idle/shell activity, and the
    // interactive/bg kind. The publisher loop re-reads them — event-driven via
    // the manifest fs-watch below, tick as backstop — so a mid-session rename
    // or turn-state flip lands on the roster. Codex has no such manifest → None
    // (its identity is hook-owned).
    let manifest = if is_codex {
        None
    } else {
        session_discovery::parent_pid().and_then(session_discovery::manifest_fields)
    };
    let session_name = manifest.as_ref().and_then(|m| m.name.clone());
    let activity = manifest.as_ref().and_then(|m| m.activity.clone());
    let kind = manifest.and_then(|m| m.kind);

    let session = Session {
        id: session_id.clone(),
        client_id: None,
        pid,
        cwd: cwd.clone(),
        git_branch: git_branch.clone(),
        current_task: None,
        session_name,
        activity,
        kind,
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
    let nicknames_cell = client.watch_query::<marshal_entities::GetAllSessionNicknames>(
        marshal_entities::GetAllSessionNicknames {},
    );

    // Mirror this session's daemon-ASSIGNED nickname to a per-session config
    // file so the runtime-free statusline (a daemon-less subcommand) shows the
    // SAME handle peers address — the daemon salts a nickname on collision, so
    // the statusline's local `nickname()` would otherwise mis-route.
    //
    // The write is REACTIVE: a subscription on the nickname view wakes the
    // writer the instant the assignment lands, so after a (re)connect the
    // statusline shows the salted handle immediately (bounded only by the WS
    // round-trip) rather than up to a poll interval later. A slow backstop tick
    // also runs so the writer re-reads the id and follows a canonical-id drift
    // (compact/clear), and as a safety net for any missed notification.
    let nicknames_for_mirror = nicknames_cell.clone();
    let session_for_mirror = Arc::clone(&session);
    let mirror_wake = Arc::new(tokio::sync::Notify::new());
    let mirror_wake_sub = Arc::clone(&mirror_wake);
    let nickname_mirror_guard = nicknames_cell.subscribe(move |_| mirror_wake_sub.notify_one());
    nicknames_cell.own(nickname_mirror_guard);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut written: Option<String> = None;
        loop {
            // Wake on either the assignment view changing (reactive, immediate)
            // or the backstop tick (drift-follow + missed-notification safety).
            tokio::select! {
                _ = interval.tick() => {}
                _ = mirror_wake.notified() => {}
            }
            let sid = session_for_mirror.lock().unwrap().id.clone();
            let assigned = nicknames_for_mirror
                .get()
                .iter()
                .find(|n| n.id.0.as_ref() == sid.0.as_ref())
                .map(|n| n.nickname.clone());
            if let Some(nick) = assigned
                && written.as_deref() != Some(nick.as_str())
            {
                write_assigned_nickname(sid.0.as_ref(), &nick);
                written = Some(nick);
            }
        }
    });

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
                    // Under Codex the shim owns no Session (the SessionStart
                    // hook registers the authoritative one) — so skip the
                    // re-SET, which would publish the placeholder id.
                    if is_codex {
                        log::info!("[marshal-shim] connected to {addr} (codex: no self-register)");
                    } else {
                        log::info!("[marshal-shim] connected to {addr} — (re)sending session");
                        let snapshot = session_for_resend.lock().unwrap().clone();
                        if let Err(e) = emit_session_set(&client_for_resend, &snapshot) {
                            log::warn!("[marshal-shim] re-SET on connect failed: {e}");
                        }
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

    // Parent-death watchdog — defense in depth beside the exit-on-EOF path.
    // Stdin EOF is the primary death signal, but it never arrives when a
    // duplicated pipe fd survives in some other process, or when a long-lived
    // app-server keeps holding the stdin of a finished conversation's server.
    // Our spawner dying reparents us; once that happens no MCP client can
    // ever reach us again, so fold into the same deregister+exit path.
    #[cfg(unix)]
    {
        // A ppid of 1 at process start means the spawner is already gone (or
        // this is a deliberately daemonized launch): reparenting is
        // undetectable there, so the watchdog would either misfire or never
        // fire — skip and rely on the exit-on-EOF path alone.
        if initial_ppid > 1 {
            let client_for_watchdog = Arc::clone(&client);
            let session_for_watchdog = Arc::clone(&session);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    let ppid = unsafe { libc::getppid() };
                    if ppid != initial_ppid {
                        log::warn!(
                            "[marshal-shim] parent {initial_ppid} exited (reparented to {ppid}); shutting down"
                        );
                        deregister_and_exit(&client_for_watchdog, &session_for_watchdog, is_codex);
                    }
                }
            });
        }
    }

    let host = Arc::new(tools::ToolHost {
        client: Arc::clone(&client),
        pid,
        cwd: cwd.clone(),
        is_codex,
        session: Arc::clone(&session),
        sessions_cell,
        rooms_cell,
        members_cell,
        nicknames_cell,
    });

    // Clone: the liveness publisher below also needs the host (for the roster
    // cell it uses to verify our own registration landed).
    let handler = Arc::new(tools::CoordHandler {
        host: Arc::clone(&host),
    });

    // Inbound-delivery differs by harness: Claude gets a live channel push,
    // Codex gets the hook-injected <marshal_inbox> block (it has no server→model
    // push). Describe the one this agent actually gets.
    let inbound_line = if is_codex {
        "Inbound direct messages appear as an injected `<marshal_inbox>` block."
    } else {
        "Inbound direct messages arrive as `notifications/claude/channel` events."
    };
    // Identity line is harness-aware. Under Codex the shim owns no session id
    // (its `session_id` here is a placeholder), so asserting it would tell the
    // agent the WRONG identity — defer to the authoritative <marshal_session>
    // block the SessionStart hook injects. Claude's shim owns the real id.
    let identity_line = if is_codex {
        format!(
            "You are a marshal-connected Codex session in {cwd}. Use the id from \
             <marshal_session> as `asSession` on writes and `?asSession=` on \
             caller-relative reads."
        )
    } else {
        format!("You are marshal session {} in {cwd}.", session_id.0)
    };
    let config = ServerConfig {
        name: "marshal-shim".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        instructions: format!(
            "{identity_line} Coordinate with sibling coding-agent sessions via Marshal.\n\
             \n\
             Read state via `marshal://` resources; write via Marshal tools.\n\
             \n\
             Address peers by nickname, session id, or id prefix. Address a human \
             by the operator email on the roster; it routes to their most-active \
             agent.\n\
             \n\
             Direct messages interrupt recipients and consume transcript context. \
             Batch related information and reserve them for action, blockers, or \
             needed replies. Use an ambient room broadcast without `@mention` for \
             FYI/progress; an `@mention` is also a direct interrupt.\n\
             \n\
             {inbound_line}",
        ),
        tools: tools::tools_def(is_codex),
        resources: tools::resources_def(is_codex),
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
    // The live roster we already subscribe to — ground truth for whether our own
    // registration actually landed (see the self-check in the publisher loop).
    let host_for_publish = Arc::clone(&host);
    // The parent `claude` pid is stable for this shim's lifetime; we poll its
    // live per-PID manifest to detect canonical-id drift (compact/clear).
    // Codex has no per-PID session manifest, and its SessionStart hook
    // (matcher covers startup|resume|clear|compact) re-registers the Session
    // on drift — so shim-side self-heal is a no-op under the Codex harness.
    let parent_pid_for_publish = if is_codex {
        None
    } else {
        session_discovery::parent_pid()
    };

    // Event-driven manifest refresh. Claude rewrites ~/.claude/sessions/<pid>.json
    // on every turn-state flip (busy↔idle) and `/rename`; an fs-watch wakes the
    // publisher the instant it changes, so `activity` and `session_name` track in
    // near real time rather than only on the 5s backstop tick. Watch the whole
    // sessions dir (the file is atomically replaced on write); extra wakes from
    // sibling sessions are cheap no-ops — the loop re-reads OURS and diffs before
    // pushing. Codex has no manifest → no watch. Debouncer leaked for the process
    // lifetime, matching the HEAD watcher above.
    let manifest_wake = Arc::new(tokio::sync::Notify::new());
    if !is_codex && let Some(dir) = session_discovery::sessions_dir() {
        use notify_debouncer_mini::{DebounceEventResult, new_debouncer, notify::RecursiveMode};
        let wake = Arc::clone(&manifest_wake);
        match new_debouncer(
            std::time::Duration::from_millis(150),
            move |_r: DebounceEventResult| wake.notify_one(),
        ) {
            Ok(mut debouncer) => {
                match debouncer.watcher().watch(&dir, RecursiveMode::NonRecursive) {
                    Ok(()) => {
                        log::info!(
                            "[marshal-shim] watching {} for turn-state changes",
                            dir.display()
                        );
                        Box::leak(Box::new(debouncer));
                    }
                    Err(e) => log::warn!("[marshal-shim] manifest watch failed: {e}"),
                }
            }
            Err(e) => log::warn!("[marshal-shim] manifest watcher init failed: {e}"),
        }
    }
    let manifest_wake_for_publish = Arc::clone(&manifest_wake);

    tokio::spawn(async move {
        // Under Codex the shim owns no Session, so there is nothing to publish
        // liveness for — the SessionStart / UserPromptSubmit hooks bump the
        // authoritative Session's activity. Skip the whole publisher.
        if is_codex {
            return;
        }
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Cache the last-pushed values so we only dispatch setters
        // that actually changed — keeps the daemon's event log from
        // accumulating no-op SETs every tick.
        let mut pushed_activity_at: Option<i64> = None;
        let mut pushed_tool: Option<String> = None;
        let mut pushed_tool_at: Option<i64> = None;
        // Seed from the value SET at registration so an unchanged name is
        // never re-pushed; only an actual `/rename` dispatches a setter.
        let mut pushed_session_name: Option<String> =
            session_for_publish.lock().unwrap().session_name.clone();
        let mut pushed_activity: Option<String> =
            session_for_publish.lock().unwrap().activity.clone();
        // Consecutive ticks our own session was absent from the roster.
        let mut missing_from_roster: u32 = 0;
        loop {
            // Wake on the 5s backstop tick OR the instant the manifest changes
            // (turn-state flip / rename), whichever comes first.
            tokio::select! {
                _ = interval.tick() => {}
                _ = manifest_wake_for_publish.notified() => {}
            }

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

            // Self-verifying registration. The drift check above depends on
            // Claude's per-PID manifest, which can silently return None, and the
            // re-SET on connect is fire-and-forget with no ack or retry. Either
            // hole leaves this session off the roster indefinitely — READS keep
            // working (they need no roster membership) while every write fails
            // "caller has no session on the roster", which reads as "marshal is
            // half-broken" rather than "I'm unregistered". The roster we already
            // subscribe to is ground truth, so check it directly and re-publish.
            {
                let (me, channels_enabled) = {
                    let s = session_for_publish.lock().unwrap();
                    (s.id.clone(), s.channels_enabled)
                };
                let sessions = host_for_publish.sessions_cell.get();
                let empty = sessions.is_empty();
                let present = sessions.iter().any(|s| s.id == me);
                // Health word for the statusline. "ok" when we're on the roster (or it
                // hasn't synced yet — the statusline's own daemon probe covers a real
                // outage); "unregistered" when the roster is populated but we're absent
                // — the send-blocked state a passive reader can't otherwise see. The
                // file mtime (rewritten here every tick) is the "shim alive" heartbeat.
                write_health(
                    me.0.as_ref(),
                    if present || empty {
                        "ok"
                    } else {
                        "unregistered"
                    },
                );
                // Live-channel state for the statusline's `no live channel` warning —
                // a DISTINCT signal from health/registration: a fully-registered
                // session can still be flag-off (forked/resumed launches bypass the
                // wrapper), which silently drops live delivery. Static per session
                // (detected once at startup); rewritten here keyed to the CURRENT id so
                // it follows a canonical-id reconcile.
                write_channels(me.0.as_ref(), channels_enabled);
                let (next, resend) = roster_miss_step(missing_from_roster, empty, present);
                missing_from_roster = next;
                if resend {
                    log::warn!(
                        "[marshal-shim] session {} absent from roster — re-SETting",
                        me.0
                    );
                    let snapshot = session_for_publish.lock().unwrap().clone();
                    if let Err(e) = emit_session_set(&client_for_publish, &snapshot) {
                        log::warn!("[marshal-shim] roster self-heal re-SET failed: {e}");
                    }
                }
            }

            // Target the CURRENT id (it may have just been reconciled above) so
            // liveness setters never write to a stale/dead session row.
            let session_id_for_publish = session_for_publish.lock().unwrap().id.clone();

            // Follow mid-session manifest changes (same file the drift check
            // above reads): the `/rename` name and the live busy/idle activity.
            // One read, push each field only when it actually changed.
            let manifest = parent_pid_for_publish.and_then(session_discovery::manifest_fields);
            let current_session_name = manifest.as_ref().and_then(|m| m.name.clone());
            let current_activity = manifest.and_then(|m| m.activity);
            if pushed_session_name != current_session_name {
                let arc_name = current_session_name.as_deref().map(Arc::<str>::from);
                let _ = client_for_publish
                    .send_command::<marshal_entities::SetSessionSessionName, ()>(
                        &marshal_entities::SetSessionSessionName {
                            id: session_id_for_publish.clone(),
                            session_name: arc_name,
                        },
                    );
                if let Ok(mut s) = session_for_publish.lock() {
                    s.session_name = current_session_name.clone();
                }
                pushed_session_name = current_session_name;
            }
            if pushed_activity != current_activity {
                let arc_activity = current_activity.as_deref().map(Arc::<str>::from);
                let _ = client_for_publish
                    .send_command::<marshal_entities::SetSessionActivity, ()>(
                        &marshal_entities::SetSessionActivity {
                            id: session_id_for_publish.clone(),
                            activity: arc_activity,
                        },
                    );
                if let Ok(mut s) = session_for_publish.lock() {
                    s.activity = current_activity.clone();
                }
                pushed_activity = current_activity;
            }

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
    let served = mcp::serve_stdio(config, handler, Arc::clone(&activity), move |notifier| {
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
    .await;

    // Stdin EOF: our MCP client is gone and no one will ever speak to us
    // again. Take our roster row down now (the daemon's staleness sweep is
    // only a lagging backstop) and exit the process.
    if let Err(e) = &served {
        log::warn!("[marshal-shim] MCP serve ended with error: {e}");
    }
    log::info!("[marshal-shim] stdin closed; deregistering and exiting");
    deregister_and_exit(&client, &session, is_codex);
}

/// Best-effort roster deregistration followed by a hard process exit.
///
/// `std::process::exit` (not a plain return) is deliberate. Returning would
/// drop the tokio runtime, whose Drop blocks on in-flight blocking-pool work
/// (tokio's stdin reader among it), and the MykoClient's OS threads would
/// keep reconnecting regardless — the process would outlive its session
/// indefinitely. Orphaned shims accumulated by the hundreds on agent exec
/// hosts until the memcg OOM killer took out unrelated services (netbird
/// included); exiting unconditionally here is one half of that fix, the
/// bounded writer drain in `mcp::serve` is the other.
fn deregister_and_exit(client: &MykoClient, session: &Arc<Mutex<Session>>, is_codex: bool) -> ! {
    // Under Codex the shim owns no Session (the SessionStart hook registers
    // the authoritative one) — nothing to deregister.
    if !is_codex && let Ok(guard) = session.lock() {
        let snapshot = guard.clone();
        drop(guard);
        if let Err(e) = emit_session_del(client, &snapshot) {
            log::warn!("[marshal-shim] roster DEL on shutdown failed: {e}");
        }
        // One beat for the client's write thread to flush the DEL frame.
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    std::process::exit(0);
}

/// SET our Session entity. Used both on initial connect and on every
/// subsequent reconnect (the daemon's in-memory store loses everything
/// when it restarts, so we have to re-publish or peers can't see us).
/// The server auto-populates `client_id` from the WS connection.
/// One tick of the roster self-check. Given the running miss count and what the
/// roster snapshot says, return `(next_miss_count, should_re_SET)`.
///
/// An empty roster means "not synced yet / disconnected", never "we're gone" —
/// concluding otherwise would re-SET on every startup before the first sync.
/// Firing resets the counter so a persistently-failing SET retries once per full
/// window instead of once per tick.
fn roster_miss_step(misses: u32, roster_empty: bool, present: bool) -> (u32, bool) {
    if roster_empty || present {
        return (0, false);
    }
    let n = misses + 1;
    if n >= ROSTER_MISS_TICKS {
        (0, true)
    } else {
        (n, false)
    }
}

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
pub(crate) fn read_address_from_config_file() -> Option<String> {
    for path in config_file_candidates(ADDRESS_FILE) {
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

/// The per-session config filename the shim mirrors its daemon-ASSIGNED
/// nickname into.
fn nickname_file_name(session_id: &str) -> String {
    format!("nickname-{session_id}")
}

/// Read the daemon-assigned nickname the shim mirrored for `session_id` (first
/// existing config-dir candidate). `None` → the caller (statusline) falls back
/// to the deterministic `marshal_entities::nickname`. This is why the
/// statusline shows the SAME handle peers address even when the daemon salted
/// this session's nickname on a collision.
pub(crate) fn read_assigned_nickname(session_id: &str) -> Option<String> {
    for path in config_file_candidates(&nickname_file_name(session_id)) {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let line = contents.lines().next().unwrap_or("").trim();
            if !line.is_empty() {
                return Some(line.to_string());
            }
        }
    }
    None
}

/// Mirror `nickname` for `session_id` to the first writable config-dir
/// candidate. Best-effort: a failure just leaves the statusline on its
/// deterministic fallback.
fn write_assigned_nickname(session_id: &str, nickname: &str) {
    let Some(path) = config_file_candidates(&nickname_file_name(session_id))
        .into_iter()
        .next()
    else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, nickname);
}

fn health_file_name(session_id: &str) -> String {
    format!("health-{session_id}")
}

/// Write this session's marshal health word for the statusline to read. The file's
/// MTIME is the freshness signal — the publisher rewrites it every tick, so a stale
/// mtime means the shim (and thus this session's MCP tools) died or hung. Best-effort.
pub(crate) fn write_health(session_id: &str, status: &str) {
    let Some(path) = config_file_candidates(&health_file_name(session_id))
        .into_iter()
        .next()
    else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, status);
}

/// Read this session's health as `(status, age)` for the statusline. `age` is time
/// since the shim last wrote it (file mtime) — a large age means the shim isn't
/// running. `None` when no file exists.
pub(crate) fn read_health(session_id: &str) -> Option<(String, std::time::Duration)> {
    for path in config_file_candidates(&health_file_name(session_id)) {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let status = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .trim()
            .to_string();
        let age = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .unwrap_or_default();
        return Some((status, age));
    }
    None
}

fn channels_file_name(session_id: &str) -> String {
    format!("channels-{session_id}")
}

/// Write this session's live-channel state for the statusline: `on` / `off` /
/// `unknown` (flag detected on / off / parent cmdline unreadable). Best-effort.
/// DISTINCT from the health file — this is a static launch property (flag off =
/// forked/resumed past the wrapper → no live delivery), not a liveness signal,
/// so the statusline can warn on it independently of health/registration.
pub(crate) fn write_channels(session_id: &str, enabled: Option<bool>) {
    let word = match enabled {
        Some(true) => "on",
        Some(false) => "off",
        None => "unknown",
    };
    let Some(path) = config_file_candidates(&channels_file_name(session_id))
        .into_iter()
        .next()
    else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, word);
}

/// Read this session's live-channel state: `Some(false)` = flag OFF (statusline
/// warns `no live channel`), `Some(true)` = on, `None` = unknown / no file yet
/// (older shim) → never warn, so we don't false-alarm on an unknown state.
pub(crate) fn read_channels(session_id: &str) -> Option<bool> {
    for path in config_file_candidates(&channels_file_name(session_id)) {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            return match contents.trim() {
                "on" => Some(true),
                "off" => Some(false),
                _ => None,
            };
        }
    }
    None
}

#[cfg(unix)]
pub(crate) fn config_file_candidates(filename: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        out.push(std::path::PathBuf::from(xdg).join("marshal").join(filename));
    }
    if let Some(home) = std::env::var_os("HOME") {
        out.push(
            std::path::PathBuf::from(home)
                .join(".config")
                .join("marshal")
                .join(filename),
        );
    }
    out
}

#[cfg(windows)]
pub(crate) fn config_file_candidates(filename: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        out.push(
            std::path::PathBuf::from(appdata)
                .join("marshal")
                .join(filename),
        );
    }
    if let Some(pd) = std::env::var_os("PROGRAMDATA") {
        out.push(std::path::PathBuf::from(pd).join("marshal").join(filename));
    }
    out
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn config_file_candidates(_filename: &str) -> Vec<std::path::PathBuf> {
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

/// Resolve which HUMAN drives this session, as an email — the canonical id
/// that links the same person across harnesses and machines. Layered,
/// strongest → weakest:
///   1. `MARSHAL_OPERATOR` — explicit override (service users / odd launches).
///   2. **Claude account email** — `~/.claude.json` `oauthAccount.emailAddress`,
///      i.e. who is *driving* this Claude Code session. Stable regardless of the
///      OS user, so it disambiguates the humans behind shared-infra `root`.
///   3. **Git `user.email`** in the workspace — who *owns* the repo (fallback
///      when there's no Claude account signal).
///   4. `$USER` / `$USERNAME` — weak; often a shared/service login (`root`).
///   5. `"anonymous"` — never fail to set an operator.
///
/// opencode sessions resolve their own account in the plugin's identity.ts;
/// this is the Claude Code path.
fn detect_operator(cwd: &str) -> String {
    if let Ok(op) = std::env::var("MARSHAL_OPERATOR") {
        let op = op.trim();
        if !op.is_empty() {
            return op.to_string();
        }
    }
    if let Some(email) = claude_account_email() {
        return email;
    }
    if let Some(email) = git_user_email(cwd) {
        return email;
    }
    std::env::var("USER")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .or_else(|| {
            std::env::var("USERNAME")
                .ok()
                .filter(|u| !u.trim().is_empty())
        })
        .unwrap_or_else(|| "anonymous".to_string())
}

/// Claude account email for the human running this Claude Code session, from
/// `~/.claude.json` `oauthAccount.emailAddress`. `None` off Claude Code
/// (opencode/other) or if the file is missing/unreadable.
fn claude_account_email() -> Option<String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let raw = std::fs::read_to_string(std::path::Path::new(&home).join(".claude.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    json.get("oauthAccount")?
        .get("emailAddress")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The workspace's git identity — `git config user.email` (effective
/// repo-or-global) run in `cwd`, i.e. who owns/configured this repo. `None`
/// when git is absent or the value is unset.
fn git_user_email(cwd: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["config", "user.email"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let email = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!email.is_empty()).then_some(email)
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
    use super::{ROSTER_MISS_TICKS, detect_git_branch, resolve_git_head_dir, roster_miss_step};
    use std::process::Command;

    #[test]
    fn roster_self_check_ignores_unsynced_and_present_rosters() {
        // Empty roster = not synced yet / disconnected. Must never be read as
        // "we were dropped", or every startup would re-SET before first sync.
        assert_eq!(roster_miss_step(0, true, false), (0, false));
        assert_eq!(roster_miss_step(5, true, false), (0, false));
        // Present = registration is intact; counter clears.
        assert_eq!(roster_miss_step(0, false, true), (0, false));
        assert_eq!(roster_miss_step(1, false, true), (0, false));
    }

    #[test]
    fn roster_self_check_reregisters_only_after_the_full_window() {
        // Absent but inside the window: count up, don't act (a just-sent SET may
        // simply not have propagated yet).
        let mut misses = 0;
        for _ in 1..ROSTER_MISS_TICKS {
            let (next, resend) = roster_miss_step(misses, false, false);
            assert!(!resend, "must not re-SET before {ROSTER_MISS_TICKS} misses");
            misses = next;
        }
        // The tick that completes the window fires and resets, so a persistently
        // failing SET retries once per window rather than every tick.
        assert_eq!(roster_miss_step(misses, false, false), (0, true));
    }

    #[test]
    fn roster_self_check_recovers_between_misses() {
        // A single miss followed by a sync must not carry over toward a re-SET.
        let (misses, resend) = roster_miss_step(0, false, false);
        assert!(!resend);
        let (misses, _) = roster_miss_step(misses, false, true);
        assert_eq!(misses, 0);
    }

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

    #[test]
    fn git_user_email_reads_the_repo_identity() {
        let tmp = std::env::temp_dir().join(format!("marshal-op-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        git(&["init", "-q"], &tmp);
        git(&["config", "user.email", "artist@studio.test"], &tmp);
        assert_eq!(
            super::git_user_email(tmp.to_str().unwrap()).as_deref(),
            Some("artist@studio.test"),
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Demonstration on the running host: the layered resolver must NOT collapse
    /// to the OS user when a Claude account / git email is present. Tolerant so
    /// CI (no account/git identity) still passes; asserts an email when a signal
    /// exists — and prints the resolved operator so it can be eyeballed.
    #[test]
    fn operator_resolves_to_a_human_not_the_os_user() {
        let op = super::detect_operator(".");
        eprintln!("detect_operator(\".\") -> {op}");
        if super::claude_account_email().is_some() || super::git_user_email(".").is_some() {
            assert!(op.contains('@'), "expected an email address, got {op:?}");
        }
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
