//! marshal-tui — terminal dashboard powered by a MykoClient.
//!
//! Connects to the daemon as a passive observer (no Session item is
//! created — peers don't see the TUI on the roster), subscribes to live
//! `GetAllSessions` and `GetAllMessages` queries via hyphae cells, and
//! renders the result with ratatui.

mod self_update;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use hyphae::{Signal, Watchable};
use marshal_entities::{
    GetAllMessages, GetAllRoomMembers, GetAllRooms, GetAllSessionNicknames, GetAllSessions,
    Message, Room, RoomKind, RoomMember, Session, SessionNickname,
};
use myko::{
    client::{ConnectionStatus, MykoClient},
    entities::client::{Client, GetAllClients},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell as RowCell, Paragraph, Row, Table},
};
use std::collections::{HashMap, HashSet};
use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

const FRAME_POLL: Duration = Duration::from_millis(150);
const RECENT_LIMIT: usize = 50;
const DEFAULT_DAEMON_ADDRESS: &str = "ws://localhost:6155";
/// Env var that overrides the daemon WebSocket URL. Same name the shim
/// uses, so a single `export MARSHAL_DAEMON_ADDRESS=...` configures
/// every marshal client. `MYKO_ADDRESS` is honored as a legacy fallback.
const ADDRESS_ENV: &str = "MARSHAL_DAEMON_ADDRESS";
const ADDRESS_ENV_LEGACY: &str = "MYKO_ADDRESS";

#[derive(Parser, Debug)]
#[command(name = "marshal-tui")]
struct Args {
    /// Override the daemon WebSocket URL. Defaults to
    /// MARSHAL_DAEMON_ADDRESS env var (then MYKO_ADDRESS for back-compat),
    /// then ws://localhost:6155.
    #[arg(long)]
    address: Option<String>,

    /// Smoke-test mode used by the self-update watcher to verify a
    /// newly-installed binary spawns cleanly before re-execing. Prints
    /// "ok" and exits 0; doesn't touch the terminal, the network, or
    /// any state. Mirrors the shim's `--check`.
    #[arg(long, hide = true)]
    check: bool,
}

#[derive(Default, Clone)]
struct StateInner {
    sessions: Vec<Arc<Session>>,
    messages: Vec<Arc<Message>>,
    rooms: Vec<Arc<Room>>,
    members: Vec<Arc<RoomMember>>,
    /// Daemon-assigned `session_id → nickname` map (the sticky
    /// `SessionNickname` store). Read-only here — the TUI never
    /// recomputes handles, same contract as the web dashboard's
    /// `nick.rs`. Sessions missing from the map (assignment saga
    /// hasn't run yet) fall back to their short id.
    nicknames: HashMap<String, String>,
    /// Live `Client` entity ids, used to render per-session connected /
    /// disconnected status. A session whose `client_id` is not in this
    /// set has lost its WS connection (or the daemon was just restarted
    /// and its shim hasn't reconnected yet).
    live_clients: HashSet<String>,
    connected: bool,
    last_event: Option<String>,
}

#[derive(Clone)]
struct State {
    inner: Arc<Mutex<StateInner>>,
}

impl State {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StateInner::default())),
        }
    }

    fn snapshot(&self) -> StateInner {
        self.inner.lock().unwrap().clone()
    }

    fn update<F: FnOnce(&mut StateInner)>(&self, f: F) {
        let mut g = self.inner.lock().unwrap();
        f(&mut g);
    }
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.check {
        println!("ok");
        return Ok(());
    }

    let address = args
        .address
        .or_else(|| std::env::var(ADDRESS_ENV).ok())
        .or_else(|| std::env::var(ADDRESS_ENV_LEGACY).ok())
        .unwrap_or_else(|| DEFAULT_DAEMON_ADDRESS.to_string());

    marshal_entities::link();

    let state = State::new();
    let client = Arc::new(MykoClient::new());

    // Self-update watcher: polls `current_exe()` mtime every 5s, runs
    // the smoke test, then re-execs into the new binary. Gated on
    // 500ms keystroke-idle so an actively-typing operator doesn't lose
    // focus mid-input. The render thread bumps `key_activity` on every
    // keypress.
    let key_activity = Arc::new(self_update::KeyActivity::new());
    self_update::spawn(Arc::clone(&key_activity));

    // Connection-status callback flips state.connected for the header dot.
    let conn_state = state.clone();
    let conn_guard = client.connection_status().subscribe(move |signal| {
        if let Signal::Value(status) = signal {
            let connected = matches!(&**status, ConnectionStatus::Connected(_));
            conn_state.update(|s| {
                s.connected = connected;
                s.last_event = Some(format!("[{}] {}", time_str(), describe(status)));
            });
        }
    });
    client.connection_status().own(conn_guard);
    client.set_address(Some(address));

    let sessions_cell = client.watch_query::<GetAllSessions>(GetAllSessions {});
    let sessions_state = state.clone();
    let sessions_guard = sessions_cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            let sessions = (**value).clone();
            sessions_state.update(|s| s.sessions = sessions);
        }
    });
    sessions_cell.own(sessions_guard);
    Box::leak(Box::new(sessions_cell));

    let clients_cell = client.watch_query::<GetAllClients>(GetAllClients {});
    let clients_state = state.clone();
    let clients_guard = clients_cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            let live: HashSet<String> = (**value)
                .iter()
                .map(|c: &Arc<Client>| c.id.0.as_ref().to_string())
                .collect();
            clients_state.update(|s| s.live_clients = live);
        }
    });
    clients_cell.own(clients_guard);
    Box::leak(Box::new(clients_cell));

    let messages_cell = client.watch_query::<GetAllMessages>(GetAllMessages {});
    let messages_state = state.clone();
    let messages_guard = messages_cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            let mut messages = (**value).clone();
            messages.sort_by_key(|m| std::cmp::Reverse(m.sent_at));
            messages.truncate(RECENT_LIMIT);
            messages_state.update(|s| s.messages = messages);
        }
    });
    messages_cell.own(messages_guard);
    Box::leak(Box::new(messages_cell));

    let rooms_cell = client.watch_query::<GetAllRooms>(GetAllRooms {});
    let rooms_state = state.clone();
    let rooms_guard = rooms_cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            rooms_state.update(|s| s.rooms = (**value).clone());
        }
    });
    rooms_cell.own(rooms_guard);
    Box::leak(Box::new(rooms_cell));

    let nicknames_cell = client.watch_query::<GetAllSessionNicknames>(GetAllSessionNicknames {});
    let nicknames_state = state.clone();
    let nicknames_guard = nicknames_cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            let map: HashMap<String, String> = (**value)
                .iter()
                .map(|n: &Arc<SessionNickname>| {
                    (n.id.0.as_ref().to_string(), n.nickname.clone())
                })
                .collect();
            nicknames_state.update(|s| s.nicknames = map);
        }
    });
    nicknames_cell.own(nicknames_guard);
    Box::leak(Box::new(nicknames_cell));

    let members_cell = client.watch_query::<GetAllRoomMembers>(GetAllRoomMembers {});
    let members_state = state.clone();
    let members_guard = members_cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            members_state.update(|s| s.members = (**value).clone());
        }
    });
    members_cell.own(members_guard);
    Box::leak(Box::new(members_cell));

    Box::leak(Box::new(client));

    let render_state = state.clone();
    let key_activity_for_render = Arc::clone(&key_activity);
    let handle = std::thread::Builder::new()
        .name("tui-render".into())
        .spawn(move || render_loop(render_state, key_activity_for_render))
        .context("spawning render thread")?;
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("render thread panicked"))?
}

fn describe(status: &ConnectionStatus) -> &'static str {
    match status {
        ConnectionStatus::Connected(_) => "connected",
        ConnectionStatus::Connecting(_) => "connecting",
        ConnectionStatus::Reconnecting(_) => "reconnecting",
        ConnectionStatus::Disconnected => "disconnected",
        ConnectionStatus::Idle => "idle",
    }
}

fn time_str() -> String {
    let t = now_ms() / 1000;
    let s = t % 60;
    let m = (t / 60) % 60;
    let h = (t / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}

fn render_loop(state: State, key_activity: Arc<self_update::KeyActivity>) -> Result<()> {
    enable_raw_mode().context("enable_raw_mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("creating terminal")?;

    let result = (|| -> Result<()> {
        loop {
            let snap = state.snapshot();
            terminal.draw(|frame| draw(&snap, frame.area(), frame))?;
            if event::poll(FRAME_POLL)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                key_activity.bump();
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    result
}

fn draw(snap: &StateInner, area: Rect, frame: &mut ratatui::Frame) {
    let chunks = Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            Constraint::Length(2),  // header
            Constraint::Min(6),     // agents
            Constraint::Length(8),  // rooms
            Constraint::Length(12), // recent messages
            Constraint::Length(1),  // status bar
        ])
        .split(area);

    draw_header(snap, chunks[0], frame);
    draw_agents(snap, chunks[1], frame);
    draw_rooms(snap, chunks[2], frame);
    draw_messages(snap, chunks[3], frame);
    draw_status(snap, chunks[4], frame);
}

fn draw_header(snap: &StateInner, area: Rect, frame: &mut ratatui::Frame) {
    let dot = if snap.connected { "●" } else { "○" };
    let dot_style = if snap.connected {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Red)
    };
    let live = snap
        .sessions
        .iter()
        .filter(|s| {
            s.client_id
                .as_ref()
                .map(|cid| snap.live_clients.contains(cid.0.as_ref()))
                .unwrap_or(false)
        })
        .count();
    let line = Line::from(vec![
        Span::styled(dot, dot_style),
        Span::raw(" marshal-tui — "),
        Span::styled(
            format!("{} sessions", snap.sessions.len()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(format!("({live} live)"), Style::default().fg(Color::Green)),
        Span::raw("  "),
        Span::styled(
            format!("{} rooms", snap.rooms.len()),
            Style::default().fg(Color::Magenta),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{} recent msgs", snap.messages.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let p = Paragraph::new(line).block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(p, area);
}

fn draw_agents(snap: &StateInner, area: Rect, frame: &mut ratatui::Frame) {
    let now = now_ms();

    let header = Row::new(vec![
        RowCell::from("conn"),
        RowCell::from("name"),
        RowCell::from("id"),
        RowCell::from("identity"),
        RowCell::from("cwd"),
        RowCell::from("branch"),
        RowCell::from("rooms"),
        RowCell::from("activity"),
        RowCell::from("uptime"),
    ])
    .style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = snap
        .sessions
        .iter()
        .map(|s| {
            let live = s
                .client_id
                .as_ref()
                .map(|cid| snap.live_clients.contains(cid.0.as_ref()))
                .unwrap_or(false);
            let conn_cell = if live {
                RowCell::from("● live").style(Style::default().fg(Color::Green))
            } else {
                RowCell::from("○ off ").style(Style::default().fg(Color::DarkGray))
            };
            let activity_cell = build_activity_cell(s, now);
            let identity = format_identity(s);
            let rooms_summary = format_member_rooms(s, &snap.members);
            let short_id: String = s.id.0.chars().take(8).collect();
            Row::new(vec![
                conn_cell,
                RowCell::from(session_label(&snap.nicknames, s.id.0.as_ref())).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                RowCell::from(short_id).style(Style::default().fg(Color::DarkGray)),
                RowCell::from(identity).style(Style::default().fg(Color::Cyan)),
                RowCell::from(short_cwd(&s.cwd)).style(Style::default().fg(Color::Gray)),
                RowCell::from(s.git_branch.clone().unwrap_or_else(|| "—".into())),
                RowCell::from(rooms_summary).style(Style::default().fg(Color::Magenta)),
                activity_cell,
                RowCell::from(format_duration(now - s.connected_at))
                    .style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(7),  // conn
            Constraint::Length(19), // name (assigned nickname)
            Constraint::Length(10), // id (session_id[:8])
            Constraint::Length(22), // identity (operator@host)
            // cwd / branch / rooms split residual width 1 : 1 : 2 —
            // rooms tends to grow fastest as ad-hoc memberships
            // accumulate, so it gets twice the share of any spare
            // terminal width. Each gets at least 8 chars before the
            // weighted split kicks in so a tiny terminal still shows
            // something readable.
            Constraint::Fill(1),    // cwd
            Constraint::Fill(1),    // branch
            Constraint::Fill(2),    // rooms
            Constraint::Length(20), // activity
            Constraint::Length(8),  // uptime
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Agents ({}) ", snap.sessions.len())),
    );
    frame.render_widget(table, area);
}

/// Replace the user's home directory in `cwd` with `~` so the column
/// stays tight in the common "I'm working in ~/Code/foo" case. Falls
/// back to the original path when `$HOME` isn't a prefix or isn't
/// resolvable.
fn short_cwd(cwd: &str) -> String {
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && cwd.starts_with(&home)
    {
        return format!("~{}", &cwd[home.len()..]);
    }
    cwd.to_string()
}

fn draw_rooms(snap: &StateInner, area: Rect, frame: &mut ratatui::Frame) {
    let header = Row::new(vec![
        RowCell::from("kind"),
        RowCell::from("room"),
        RowCell::from("name"),
        RowCell::from("members"),
    ])
    .style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    // Auto-rooms first (everyone always at the very top so it's
    // easy to spot in any roster size), then ad-hoc.
    let mut sorted: Vec<&Arc<Room>> = snap.rooms.iter().collect();
    sorted.sort_by_key(|r| {
        let auto_rank = match &r.kind {
            RoomKind::Auto { .. } => 0,
            RoomKind::Adhoc => 1,
        };
        let everyone_first = if r.id.0.as_ref() == "everyone" { 0 } else { 1 };
        (auto_rank, everyone_first, r.id.0.as_ref().to_string())
    });

    let rows: Vec<Row> = sorted
        .iter()
        .map(|r| {
            let count = snap.members.iter().filter(|m| m.room_id == r.id).count();
            let kind_label = match &r.kind {
                RoomKind::Auto { .. } => {
                    RowCell::from("auto").style(Style::default().fg(Color::Cyan))
                }
                RoomKind::Adhoc => RowCell::from("adhoc").style(Style::default().fg(Color::Yellow)),
            };
            Row::new(vec![
                kind_label,
                RowCell::from(r.id.0.as_ref().to_string()).style(
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(Color::Magenta),
                ),
                RowCell::from(r.name.clone()).style(Style::default().fg(Color::Gray)),
                RowCell::from(format!("{count}")).style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(28),
            Constraint::Min(20),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Rooms ({}) ", snap.rooms.len())),
    );
    frame.render_widget(table, area);
}

/// Format a session's identity column as `operator@host` — the two
/// fields the AutoRoomSaga keys auto-rooms on. Falls back to `?` for
/// missing pieces (older shims, observer-only TUIs) so the column
/// doesn't go blank.
fn format_identity(s: &Session) -> String {
    let op = s.operator.as_deref().unwrap_or("?");
    let host = s.host.as_ref().map(|h| h.name.as_str()).unwrap_or("?");
    format!("{op}@{host}")
}

/// Build a session's room-membership summary. Auto-rooms keep their
/// `host:` / `op:` / `project:` prefix so the operator can tell at a
/// glance which auto-anchors apply; ad-hoc rooms just show their id.
/// `everyone` is omitted (every session is in it; showing it on every
/// row is noise).
fn format_member_rooms(s: &Session, members: &[Arc<RoomMember>]) -> String {
    let mut ids: Vec<&str> = members
        .iter()
        .filter(|m| m.session_id == s.id)
        .map(|m| m.room_id.0.as_ref())
        .filter(|id| *id != "everyone")
        .collect();
    if ids.is_empty() {
        return "—".into();
    }
    ids.sort();
    ids.join(", ")
}

/// Eight-char short form of a session_id, used as the fallback label
/// wherever no nickname assignment has landed yet. Session_id is the
/// sole stored identity; readable labels are composed at view time.
fn short_sid(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A session's display label: its daemon-assigned nickname, or the
/// short id in the window before the assignment saga has run.
fn session_label(nicknames: &HashMap<String, String>, session_id: &str) -> String {
    nicknames
        .get(session_id)
        .cloned()
        .unwrap_or_else(|| short_sid(session_id))
}

fn draw_messages(snap: &StateInner, area: Rect, frame: &mut ratatui::Frame) {
    let now = now_ms();

    let lines: Vec<Line> = snap
        .messages
        .iter()
        .map(|m| {
            // Broadcasts (to_room_id set) render with a `#` prefix on the
            // recipient and a brighter arrow so the eye picks them out
            // of the direct-send stream. Direct sends just show the
            // recipient's short session_id.
            let is_broadcast = m.to_room_id.is_some();
            let arrow = if is_broadcast { " ⇒ " } else { " → " };
            let recipient_label = if let Some(rid) = m.to_room_id.as_ref() {
                format!("#{}", short_sid(rid.0.as_ref()))
            } else if let Some(sid) = m.to_session_id.as_ref() {
                session_label(&snap.nicknames, sid.0.as_ref())
            } else {
                "?".to_string()
            };
            let recipient_style = if is_broadcast {
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                Span::styled(
                    format!("{:>5}  ", format_duration(now - m.sent_at)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!(
                        "{:>18}",
                        session_label(&snap.nicknames, m.from_session_id.0.as_ref())
                    ),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    arrow,
                    if is_broadcast {
                        Style::default().fg(Color::Magenta)
                    } else {
                        Style::default().fg(Color::Cyan)
                    },
                ),
                Span::styled(format!("{:<18}  ", recipient_label), recipient_style),
                Span::raw(m.body.replace('\n', " ⏎ ")),
            ])
        })
        .collect();

    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Recent messages ({}) ", snap.messages.len())),
    );
    frame.render_widget(p, area);
}

fn draw_status(snap: &StateInner, area: Rect, frame: &mut ratatui::Frame) {
    let last = snap
        .last_event
        .clone()
        .unwrap_or_else(|| "(no events yet)".into());
    let p = Paragraph::new(Line::from(vec![
        Span::styled(last, Style::default().fg(Color::DarkGray)),
        Span::raw("    "),
        Span::styled("q quit", Style::default().fg(Color::DarkGray)),
    ]));
    frame.render_widget(p, area);
}

/// Build the agents-table activity cell from a session's liveness
/// fields (`last_activity_at`, `last_tool`, `last_tool_at`). Color
/// drains from green → yellow → red as `last_activity_at` ages past
/// 60s and 5min — same thresholds the web SessionsCard uses, so an
/// operator switching between web and TUI sees consistent color
/// language. A session with no recorded activity yet (newly
/// connected, hasn't pushed its first liveness flush) renders as
/// dim "—" rather than implying it's stuck.
fn build_activity_cell<'a>(s: &Session, now_ms: i64) -> RowCell<'a> {
    let style = match s.last_activity_at {
        None => Style::default().fg(Color::DarkGray),
        Some(ts) => {
            let age_ms = now_ms.saturating_sub(ts);
            if age_ms < 60_000 {
                Style::default().fg(Color::Green)
            } else if age_ms < 300_000 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            }
        }
    };
    let body = match (s.last_tool.as_deref(), s.last_tool_at, s.last_activity_at) {
        (Some(tool), Some(at), _) => {
            format!("{tool} {}", format_age_short(now_ms.saturating_sub(at)))
        }
        (None, _, Some(at)) => {
            format!("— {}", format_age_short(now_ms.saturating_sub(at)))
        }
        (None, _, None) => "—".into(),
        (Some(tool), None, _) => tool.to_string(),
    };
    RowCell::from(body).style(style)
}

fn format_age_short(ms: i64) -> String {
    if ms <= 0 {
        return "0s".into();
    }
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn format_duration(diff_ms: i64) -> String {
    if diff_ms < 0 {
        return "0s".into();
    }
    let s = diff_ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}
