//! claude-coord-tui — terminal dashboard powered by a MykoClient.
//!
//! Connects to the daemon as a passive observer (no Session item is
//! created — peers don't see the TUI on the roster), subscribes to live
//! `GetAllSessions` and `GetAllMessages` queries via hyphae cells, and
//! renders the result with ratatui.

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use entities::{GetAllMessages, GetAllSessions, Message, Session};
use hyphae::{Signal, Watchable};
use myko::{
    client::{ConnectionStatus, MykoClient},
    entities::client::{Client, GetAllClients},
};
use std::collections::HashSet;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell as RowCell, Paragraph, Row, Table},
};
use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

const FRAME_POLL: Duration = Duration::from_millis(150);
const RECENT_LIMIT: usize = 50;
const DEFAULT_MYKO_ADDRESS: &str = "ws://localhost:6155";

#[derive(Parser, Debug)]
#[command(name = "claude-coord-tui")]
struct Args {
    /// Override the daemon WebSocket URL. Defaults to MYKO_ADDRESS env var,
    /// then to ws://localhost:6155.
    #[arg(long)]
    address: Option<String>,
}

#[derive(Default, Clone)]
struct StateInner {
    sessions: Vec<Arc<Session>>,
    messages: Vec<Arc<Message>>,
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
    let address = args
        .address
        .or_else(|| std::env::var("MYKO_ADDRESS").ok())
        .unwrap_or_else(|| DEFAULT_MYKO_ADDRESS.to_string());

    entities::link();

    let state = State::new();
    let client = Arc::new(MykoClient::new());

    // Connection-status callback flips state.connected for the header dot.
    let conn_state = state.clone();
    let conn_guard = client.connection_status().subscribe(move |signal| {
        if let Signal::Value(status) = signal {
            let connected = matches!(&**status, ConnectionStatus::Connected(_));
            conn_state.update(|s| {
                s.connected = connected;
                s.last_event = Some(format!("[{}] {}", time_str(), describe(&status)));
            });
        }
    });
    client.connection_status().own(conn_guard);
    client.set_address(Some(address));

    // Live query subscriptions. The cells start empty and refill as the daemon
    // streams query responses; we drain them into our shared state on every
    // signal.
    let sessions_cell = client.watch_query::<GetAllSessions>(GetAllSessions {});
    let sessions_state = state.clone();
    let sessions_guard = sessions_cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            let sessions = (**value).clone();
            sessions_state.update(|s| s.sessions = sessions);
        }
    });
    sessions_cell.own(sessions_guard);
    // Keep the cell alive for the lifetime of the process.
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
            // Newest first, capped to RECENT_LIMIT for the renderer.
            messages.sort_by_key(|m| std::cmp::Reverse(m.sent_at));
            messages.truncate(RECENT_LIMIT);
            messages_state.update(|s| s.messages = messages);
        }
    });
    messages_cell.own(messages_guard);
    Box::leak(Box::new(messages_cell));

    // The MykoClient must outlive both the subscription guards and the render
    // loop — once dropped, its WebSocket reader stops and the cells stop
    // updating. We leak it on purpose: the process owns it for its lifetime.
    Box::leak(Box::new(client));

    // Render in a dedicated OS thread; ratatui + crossterm don't need tokio.
    let render_state = state.clone();
    let handle = std::thread::Builder::new()
        .name("tui-render".into())
        .spawn(move || render_loop(render_state))
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

fn render_loop(state: State) -> Result<()> {
    enable_raw_mode().context("enable_raw_mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("creating terminal")?;

    let result = (|| -> Result<()> {
        loop {
            let snap = state.snapshot();
            terminal.draw(|frame| draw(&snap, frame.area(), frame))?;
            if event::poll(FRAME_POLL)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('c')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                break;
                            }
                            _ => {}
                        }
                    }
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
            Constraint::Min(8),     // agents
            Constraint::Length(14), // recent messages
            Constraint::Length(1),  // status bar
        ])
        .split(area);

    draw_header(snap, chunks[0], frame);
    draw_agents(snap, chunks[1], frame);
    draw_messages(snap, chunks[2], frame);
    draw_status(snap, chunks[3], frame);
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
        Span::raw(" claude-coord-tui — "),
        Span::styled(
            format!("{} sessions", snap.sessions.len()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("({live} live)"),
            Style::default().fg(Color::Green),
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
        RowCell::from("nick"),
        RowCell::from("session"),
        RowCell::from("role"),
        RowCell::from("cwd"),
        RowCell::from("branch"),
        RowCell::from("status"),
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
            let role_cell = match s.role.as_deref() {
                Some(r) => RowCell::from(r.to_string())
                    .style(Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
                None => RowCell::from("—").style(Style::default().fg(Color::DarkGray)),
            };
            // A session is "live" if its bound client_id resolves to a
            // Client in the current GetAllClients snapshot. Sessions with
            // no client_id at all (e.g. just-replayed from disk before a
            // shim has rebound) also count as disconnected.
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
            Row::new(vec![
                conn_cell,
                RowCell::from(s.nickname.clone())
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                RowCell::from(s.id.to_string()).style(Style::default().fg(Color::Cyan)),
                role_cell,
                RowCell::from(s.cwd.clone()).style(Style::default().fg(Color::Gray)),
                RowCell::from(s.git_branch.clone().unwrap_or_else(|| "—".into())),
                RowCell::from(s.current_task.clone().unwrap_or_else(|| "—".into())),
                RowCell::from(format_duration(now - s.connected_at))
                    .style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(16),
            Constraint::Length(14),
            Constraint::Length(16),
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(20),
            Constraint::Length(8),
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

fn draw_messages(snap: &StateInner, area: Rect, frame: &mut ratatui::Frame) {
    let now = now_ms();

    let lines: Vec<Line> = snap
        .messages
        .iter()
        .map(|m| {
            Line::from(vec![
                Span::styled(
                    format!("{:>5}  ", format_duration(now - m.sent_at)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:>14}", m.from_nick),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" → ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{:<14}  ", m.to_nick),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
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
