//! claude-coord-tui — realtime terminal dashboard for the claude-coord daemon.
//!
//! Connects to the daemon over its unix socket as an ordinary "tui" session,
//! periodically polls roster / recent_messages, and updates a shared
//! `Mutex<StateInner>` that the ratatui render loop reads on every frame.
//! Push events from the daemon (joined / agent_joined / new_message) are also
//! applied optimistically so the screen reacts before the next poll.

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use proto::framing::{read_frame, write_frame};
use proto::messages::{ClientMsg, ServerMsg, SessionInfo};
use proto::rpc::{
    method, Direction, RecentMessage, RecentMessagesParams, RecentMessagesResult, RosterParams,
    RosterResult,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell as RowCell, Paragraph, Row, Table},
    Terminal,
};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

const POLL_INTERVAL: Duration = Duration::from_millis(2500);
const RECENT_LIMIT: u32 = 50;
const FRAME_POLL: Duration = Duration::from_millis(150);

#[derive(Parser, Debug)]
#[command(name = "claude-coord-tui")]
struct Args {
    /// Override the daemon socket path. Defaults to $XDG_RUNTIME_DIR/claude-coord/sock.
    #[arg(long)]
    socket: Option<PathBuf>,
}

fn socket_path() -> PathBuf {
    if let Some(rd) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(rd).join("claude-coord/sock");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/claude-coord/sock");
    }
    PathBuf::from("/tmp/claude-coord/sock")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Default, Clone)]
struct StateInner {
    me_session_id: Option<String>,
    me_nickname: Option<String>,
    roster: Vec<SessionInfo>,
    recent: Vec<RecentMessage>,
    last_event: Option<String>,
    connected: bool,
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let socket = args.socket.unwrap_or_else(socket_path);
    let state = State::new();

    // Network task: reconnects forever, runs hello/welcome + poll loop.
    let net_state = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_connection(&socket, &net_state).await {
                net_state.update(|s| s.last_event = Some(format!("disconnected: {e}")));
            }
            net_state.update(|s| s.connected = false);
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    });

    // Render in a blocking thread; ratatui + crossterm don't need tokio.
    let render_state = state.clone();
    let handle = std::thread::Builder::new()
        .name("tui-render".into())
        .spawn(move || render_loop(render_state))
        .context("spawning render thread")?;
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("render thread panicked"))?
}

async fn run_connection(socket: &PathBuf, state: &State) -> Result<()> {
    let sock = UnixStream::connect(socket)
        .await
        .context("connecting to daemon")?;
    let (read_half, write_half) = sock.into_split();
    let mut read_half = read_half;
    let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

    // Hello.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let hello = ClientMsg::Hello {
        pid: std::process::id(),
        cwd,
        git_branch: None,
    };
    write_frame(
        &mut *write_half.lock().await,
        &serde_json::to_vec(&hello)?,
    )
    .await?;

    // Welcome.
    let frame = read_frame(&mut read_half).await?;
    let resp: ServerMsg = serde_json::from_slice(&frame)?;
    let (session_id, nickname) = match resp {
        ServerMsg::Welcome {
            session_id,
            nickname,
        } => (session_id, nickname),
        other => anyhow::bail!("expected welcome, got {other:?}"),
    };
    state.update(|s| {
        s.me_session_id = Some(session_id);
        s.me_nickname = Some(nickname);
        s.connected = true;
        s.last_event = Some(format!("[{}] connected", time_str()));
    });

    // RPC plumbing.
    let next_id = Arc::new(AtomicU64::new(1));
    let (reply_tx, mut reply_rx) =
        mpsc::unbounded_channel::<(u64, std::result::Result<serde_json::Value, String>)>();

    let reader_state = state.clone();
    let reader_handle = tokio::spawn(async move {
        loop {
            let frame = match read_frame(&mut read_half).await {
                Ok(f) => f,
                Err(_) => break,
            };
            let msg: ServerMsg = match serde_json::from_slice(&frame) {
                Ok(m) => m,
                Err(_) => continue,
            };
            match msg {
                ServerMsg::RpcOk { id, result } => {
                    let _ = reply_tx.send((id, Ok(result)));
                }
                ServerMsg::RpcErr { id, message, .. } => {
                    let _ = reply_tx.send((id, Err(message)));
                }
                ServerMsg::Event { kind, payload } => {
                    handle_event(&reader_state, &kind, &payload);
                }
                ServerMsg::Welcome { .. } => {}
            }
        }
    });

    // Poll loop.
    loop {
        match call::<RosterResult>(
            &write_half,
            &next_id,
            method::ROSTER,
            serde_json::to_value(RosterParams::default())?,
            &mut reply_rx,
        )
        .await
        {
            Ok(r) => state.update(|s| s.roster = r.sessions),
            Err(e) => {
                state.update(|s| s.last_event = Some(format!("roster failed: {e}")));
                break;
            }
        }
        match call::<RecentMessagesResult>(
            &write_half,
            &next_id,
            method::RECENT_MESSAGES,
            serde_json::to_value(RecentMessagesParams { limit: RECENT_LIMIT })?,
            &mut reply_rx,
        )
        .await
        {
            Ok(r) => state.update(|s| s.recent = r.messages),
            Err(e) => {
                state.update(|s| s.last_event = Some(format!("recent failed: {e}")));
                break;
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    reader_handle.abort();
    Ok(())
}

async fn call<R: serde::de::DeserializeOwned>(
    write_half: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    next_id: &Arc<AtomicU64>,
    method: &str,
    params: serde_json::Value,
    reply_rx: &mut mpsc::UnboundedReceiver<(u64, std::result::Result<serde_json::Value, String>)>,
) -> Result<R> {
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let req = ClientMsg::Rpc {
        id,
        method: method.into(),
        params,
    };
    let buf = serde_json::to_vec(&req)?;
    write_frame(&mut *write_half.lock().await, &buf).await?;

    while let Some((rid, res)) = reply_rx.recv().await {
        if rid != id {
            continue;
        }
        return match res {
            Ok(v) => Ok(serde_json::from_value(v)?),
            Err(e) => Err(anyhow::anyhow!("rpc {method} failed: {e}")),
        };
    }
    Err(anyhow::anyhow!("reply channel closed for {method}"))
}

fn handle_event(state: &State, kind: &str, payload: &serde_json::Value) {
    let summary = match kind {
        "joined" => format!("joined as {}", payload["nickname"].as_str().unwrap_or("?")),
        "agent_joined" => format!(
            "agent joined: {}",
            payload["nickname"].as_str().unwrap_or("?")
        ),
        "new_message" => format!(
            "msg from {}: {}",
            payload["from_nick"].as_str().unwrap_or("?"),
            payload["body"].as_str().unwrap_or("")
        ),
        other => format!("event {other}"),
    };

    state.update(|s| {
        s.last_event = Some(format!("[{}] {summary}", time_str()));
        if kind == "new_message" {
            // Optimistic prepend so the message appears before the next poll.
            let me = s.me_session_id.clone().unwrap_or_default();
            let from_session = payload["from_session"].as_str().unwrap_or("").to_string();
            let direction = if from_session == me {
                Direction::Sent
            } else {
                Direction::Received
            };
            let msg = RecentMessage {
                message: proto::messages::Message {
                    id: payload["message_id"].as_i64().unwrap_or(0),
                    from_session,
                    from_nick: payload["from_nick"].as_str().unwrap_or("?").into(),
                    body: payload["body"].as_str().unwrap_or("").into(),
                    sent_at: payload["sent_at"].as_i64().unwrap_or_else(now_ms),
                },
                direction,
                to_nick: payload["to_nick"].as_str().unwrap_or("?").into(),
            };
            s.recent.insert(0, msg);
            if s.recent.len() > RECENT_LIMIT as usize {
                s.recent.truncate(RECENT_LIMIT as usize);
            }
        }
    });
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
                                break
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
    let nick = snap
        .me_nickname
        .clone()
        .unwrap_or_else(|| "(connecting…)".into());
    let id = snap.me_session_id.clone().unwrap_or_default();
    let dot = if snap.connected { "●" } else { "○" };
    let dot_style = if snap.connected {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Red)
    };
    let line = Line::from(vec![
        Span::styled(dot, dot_style),
        Span::raw(" claude-coord — "),
        Span::styled(nick, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(" ({id})")),
    ]);
    let p = Paragraph::new(line).block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(p, area);
}

fn draw_agents(snap: &StateInner, area: Rect, frame: &mut ratatui::Frame) {
    let me_id = snap.me_session_id.clone().unwrap_or_default();
    let now = now_ms();

    let header = Row::new(vec![
        RowCell::from("nick"),
        RowCell::from("session"),
        RowCell::from("cwd"),
        RowCell::from("branch"),
        RowCell::from("status"),
        RowCell::from("idle"),
    ])
    .style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = snap
        .roster
        .iter()
        .map(|s| {
            let is_self = s.session_id == me_id;
            let nick_style = if is_self {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            Row::new(vec![
                RowCell::from(s.nickname.clone()).style(nick_style),
                RowCell::from(s.session_id.clone()).style(Style::default().fg(Color::Cyan)),
                RowCell::from(s.cwd.display().to_string()).style(Style::default().fg(Color::Gray)),
                RowCell::from(s.git_branch.clone().unwrap_or_else(|| "—".into())),
                RowCell::from(s.current_task.clone().unwrap_or_else(|| "—".into())),
                RowCell::from(format_idle(now - s.last_heartbeat))
                    .style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(12),
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
            .title(format!(" Agents ({}) ", snap.roster.len())),
    );
    frame.render_widget(table, area);
}

fn draw_messages(snap: &StateInner, area: Rect, frame: &mut ratatui::Frame) {
    let me_id = snap.me_session_id.clone().unwrap_or_default();
    let now = now_ms();

    let lines: Vec<Line> = snap
        .recent
        .iter()
        .map(|m| {
            let is_self_sent = m.message.from_session == me_id;
            Line::from(vec![
                Span::styled(
                    format!("{:>5}  ", format_idle(now - m.message.sent_at)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:>14}", m.message.from_nick),
                    Style::default()
                        .fg(if is_self_sent {
                            Color::Yellow
                        } else {
                            Color::Magenta
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " → ",
                    if is_self_sent {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::Cyan)
                    },
                ),
                Span::styled(
                    format!("{:<14}  ", m.to_nick),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw(m.message.body.replace('\n', " ⏎ ")),
            ])
        })
        .collect();

    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Recent messages ({}) ", snap.recent.len())),
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

fn format_idle(diff_ms: i64) -> String {
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
