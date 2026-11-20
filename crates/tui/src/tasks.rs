//! Live project board built from the server's task, blocker, and claim rows.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use crossterm::event::KeyCode;
use hyphae::Gettable;
use marshal_client::MarshalClient;
use marshal_entities::{
    DispatchAttempt, DispatchHarness, DispatchHost, DispatchState, GetAllDispatchAttempts,
    GetAllDispatchHosts, GetAllTaskBlockers, GetAllTaskClaims, GetAllTasks, ReserveTaskDispatch,
    Task, TaskBlocker, TaskClaim, TaskPriority, TaskStatus,
};
use myko::client::QueryWatch;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};

use crate::{PaneScroll, now_ms};

pub struct TaskBoard {
    client: MarshalClient,
    hosts: QueryWatch<DispatchHost>,
    tasks: QueryWatch<Task>,
    blockers: QueryWatch<TaskBlocker>,
    claims: QueryWatch<TaskClaim>,
    dispatches: QueryWatch<DispatchAttempt>,
}

impl TaskBoard {
    pub fn subscribe(client: &MarshalClient) -> Self {
        Self {
            client: client.clone(),
            hosts: client.watch_query_state::<GetAllDispatchHosts>(GetAllDispatchHosts {}),
            tasks: client.watch_query_state::<GetAllTasks>(GetAllTasks {}),
            blockers: client.watch_query_state::<GetAllTaskBlockers>(GetAllTaskBlockers {}),
            claims: client.watch_query_state::<GetAllTaskClaims>(GetAllTaskClaims {}),
            dispatches: client
                .watch_query_state::<GetAllDispatchAttempts>(GetAllDispatchAttempts {}),
        }
    }

    fn snapshot(&self) -> Result<Option<Snapshot>, String> {
        for error in [
            self.tasks.error(),
            self.blockers.error(),
            self.claims.error(),
            self.dispatches.error(),
        ] {
            if let Some(error) = error.get() {
                return Err(error);
            }
        }
        Ok((self.tasks.ready().get()
            && self.blockers.ready().get()
            && self.claims.ready().get()
            && self.dispatches.ready().get())
        .then(|| Snapshot {
            tasks: self.tasks.items().get(),
            blockers: self.blockers.items().get(),
            claims: self.claims.items().get(),
            dispatches: self.dispatches.items().get(),
        }))
    }
}

struct Snapshot {
    tasks: Vec<Arc<Task>>,
    blockers: Vec<Arc<TaskBlocker>>,
    claims: Vec<Arc<TaskClaim>>,
    dispatches: Vec<Arc<DispatchAttempt>>,
}

#[derive(Default)]
pub struct View {
    project: Option<String>,
    scroll: PaneScroll,
    dispatch: Option<DispatchForm>,
    notice: Option<String>,
}

type DispatchReply = hyphae::Cell<Option<Result<DispatchAttempt, String>>, hyphae::CellImmutable>;

struct DispatchForm {
    tasks: Vec<Arc<Task>>,
    targets: Vec<(String, DispatchHarness)>,
    task: usize,
    target: usize,
    workspace: String,
    response: Option<DispatchReply>,
    error: Option<String>,
}

impl View {
    pub fn is_dispatching(&self) -> bool {
        self.dispatch.is_some()
    }

    fn open_dispatch(&mut self, board: &TaskBoard) {
        if let Some(error) = board.hosts.error().get() {
            self.notice = Some(error);
            return;
        }
        let Ok(Some(snapshot)) = board.snapshot() else {
            return;
        };
        let mut tasks: Vec<_> = snapshot
            .tasks
            .into_iter()
            .filter(|task| {
                task.status == TaskStatus::Open
                    && self
                        .project
                        .as_ref()
                        .is_none_or(|project| project == &task.project)
            })
            .collect();
        tasks.sort_unstable_by(|a, b| {
            (&a.project, a.priority, a.created, &a.id.0)
                .cmp(&(&b.project, b.priority, b.created, &b.id.0))
        });
        let mut hosts = board.hosts.items().get();
        hosts.sort_unstable_by(|a, b| a.id.0.cmp(&b.id.0));
        let targets: Vec<_> = hosts
            .iter()
            .flat_map(|host| {
                host.launch_harnesses
                    .iter()
                    .map(|&harness| (host.id.to_string(), harness))
            })
            .collect();
        if tasks.is_empty() || targets.is_empty() {
            self.notice = Some(
                if tasks.is_empty() {
                    "No open tasks"
                } else {
                    "No launch-enabled hosts connected"
                }
                .into(),
            );
            return;
        }
        self.dispatch = Some(DispatchForm {
            tasks,
            targets,
            task: 0,
            target: 0,
            workspace: String::new(),
            response: None,
            error: None,
        });
    }

    fn dispatch_key(&mut self, board: &TaskBoard, key: KeyCode) {
        let Some(form) = self.dispatch.as_mut() else {
            return;
        };
        if form.response.is_some() {
            return;
        }
        if key == KeyCode::Esc {
            self.dispatch = None;
            return;
        }
        match key {
            KeyCode::Up => form.task = (form.task + form.tasks.len() - 1) % form.tasks.len(),
            KeyCode::Down => form.task = (form.task + 1) % form.tasks.len(),
            KeyCode::Left => {
                form.target = (form.target + form.targets.len() - 1) % form.targets.len()
            }
            KeyCode::Right => form.target = (form.target + 1) % form.targets.len(),
            KeyCode::Backspace => {
                form.workspace.pop();
            }
            KeyCode::Char(ch)
                if !ch.is_control() && form.workspace.len() + ch.len_utf8() <= 4096 =>
            {
                form.workspace.push(ch)
            }
            KeyCode::Enter if !form.workspace.trim().is_empty() => {
                let (host_id, harness) = &form.targets[form.target];
                form.error = None;
                form.response = Some(
                    board
                        .client
                        .send_command::<ReserveTaskDispatch, DispatchAttempt>(
                            &ReserveTaskDispatch {
                                task_id: form.tasks[form.task].id.clone(),
                                host_id: host_id.clone(),
                                harness: *harness,
                                workspace: form.workspace.clone(),
                            },
                        ),
                );
            }
            _ => {}
        }
    }

    pub fn key(&mut self, board: &TaskBoard, key: KeyCode) {
        if self.is_dispatching() {
            self.dispatch_key(board, key);
            return;
        }
        if key == KeyCode::Char('d') {
            self.open_dispatch(board);
            return;
        }
        if matches!(key, KeyCode::Char('[') | KeyCode::Char(']')) {
            if let Ok(Some(snapshot)) = board.snapshot() {
                let projects = snapshot.projects();
                let choices: Vec<_> = std::iter::once(None)
                    .chain(projects.keys().copied().map(Some))
                    .collect();
                let selected = choices
                    .iter()
                    .position(|project| *project == self.project.as_deref())
                    .unwrap_or(0);
                let step = if key == KeyCode::Char(']') {
                    1
                } else {
                    choices.len() - 1
                };
                self.project = choices[(selected + step) % choices.len()].map(str::to_owned);
                self.scroll.offset = 0;
            }
            return;
        }
        self.scroll.key(key);
    }

    pub fn draw(&mut self, board: &TaskBoard, area: Rect, frame: &mut Frame) {
        if let Some(form) = self.dispatch.as_mut()
            && let Some(result) = form.response.as_ref().and_then(|response| response.get())
        {
            form.response = None;
            match result {
                Ok(attempt) => {
                    self.notice = Some(format!("Dispatch reserved on {}", attempt.host_id));
                    self.dispatch = None;
                }
                Err(error) => form.error = Some(error),
            }
        }
        if let Some(form) = &self.dispatch {
            let task = &form.tasks[form.task];
            let (host, harness) = &form.targets[form.target];
            let harness = match harness {
                DispatchHarness::Claude => "Claude",
                DispatchHarness::Codex => "Codex",
            };
            let status = if form.response.is_some() {
                "Reserving dispatch…"
            } else {
                form.error
                    .as_deref()
                    .unwrap_or("↑↓ task   ←→ host and harness   Enter dispatch   Esc back")
            };
            frame.render_widget(
                Paragraph::new(format!(
                    "Task       {} / {}\nHost       {host} ({harness})\nWorkspace  {}\n\n{status}",
                    task.project, task.title, form.workspace,
                ))
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(board_block("Dispatch task")),
                area,
            );
            return;
        }
        let [board_area, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        frame.render_widget(
            Paragraph::new(
                self.notice
                    .as_deref()
                    .unwrap_or("t roster   [ / ] project   ↑↓ scroll   d dispatch   q quit"),
            )
            .style(Style::default().fg(Color::Gray)),
            footer,
        );
        match board.snapshot() {
            Ok(Some(snapshot)) => self.draw_rows(&snapshot, board_area, frame),
            Ok(None) => frame.render_widget(
                Paragraph::new("Waiting for server task data…").block(board_block("Tasks")),
                board_area,
            ),
            Err(error) => frame.render_widget(
                Paragraph::new(format!("Task data unavailable: {error}"))
                    .block(board_block("Tasks")),
                board_area,
            ),
        }
    }

    fn draw_rows(&mut self, snapshot: &Snapshot, area: Rect, frame: &mut Frame) {
        let projects = snapshot.projects();
        if self
            .project
            .as_ref()
            .is_some_and(|project| !projects.contains_key(project.as_str()))
        {
            self.project = None;
            self.scroll.offset = 0;
        }
        let filter = self.project.as_deref();
        let title = format!("Tasks · {}", filter.unwrap_or("All projects"));
        if projects.is_empty() {
            frame.render_widget(
                Paragraph::new("No tasks yet").block(board_block(title)),
                area,
            );
            return;
        }

        let active_claims: HashMap<_, _> = snapshot
            .claims
            .iter()
            .filter(|claim| claim.expires > now_ms())
            .map(|claim| (&claim.task_id, claim.session_id.0.as_ref()))
            .collect();
        let closed: HashSet<_> = snapshot
            .tasks
            .iter()
            .filter(|task| matches!(task.status, TaskStatus::Closed { .. }))
            .map(|task| &task.id)
            .collect();
        let blocked: HashSet<_> = snapshot
            .blockers
            .iter()
            .filter(|edge| !closed.contains(&edge.blocker_id))
            .map(|edge| &edge.task_id)
            .collect();

        let dispatches: HashMap<_, _> = snapshot
            .dispatches
            .iter()
            .map(|attempt| (&attempt.task_id, &attempt.state))
            .collect();

        let mut rows = Vec::new();
        for (project, mut tasks) in projects {
            if filter.is_some_and(|selected| selected != project) {
                continue;
            }
            let state = |task: &Task| match task.status {
                TaskStatus::Closed { .. } => State::Closed,
                TaskStatus::Open if active_claims.contains_key(&task.id) => State::Claimed,
                TaskStatus::Open if blocked.contains(&task.id) => State::Blocked,
                TaskStatus::Open => match dispatches.get(&task.id) {
                    Some(DispatchState::Reserved | DispatchState::Starting) => State::Launching,
                    Some(DispatchState::Running { .. }) => State::Started,
                    Some(DispatchState::Joined { .. }) => State::Joined,
                    Some(DispatchState::Offered { .. }) => State::Offered,
                    Some(DispatchState::Delivered { .. }) => State::Delivered,
                    Some(DispatchState::Claimed { .. }) => State::Ready,
                    Some(DispatchState::Rejected { .. }) => State::LaunchFailed,
                    None => State::Ready,
                },
            };
            tasks.sort_by_key(|task| (state(task), task.priority, task.created, &task.id.0));
            rows.push(
                Row::new(["", "  ", project, ""]).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            );
            rows.extend(tasks.into_iter().map(|task| {
                let state = state(task);
                let priority = match task.priority {
                    TaskPriority::P0 => "P0",
                    TaskPriority::P1 => "P1",
                    TaskPriority::P2 => "P2",
                    TaskPriority::P3 => "P3",
                };
                Row::new([
                    Cell::from(state.label()).style(Style::default().fg(state.color())),
                    Cell::from(priority),
                    Cell::from(task.title.as_str()),
                    Cell::from(if state == State::Claimed {
                        active_claims[&task.id]
                    } else {
                        ""
                    }),
                ])
            }));
        }
        let page = usize::from(area.height.saturating_sub(3));
        self.scroll
            .update(rows.len().saturating_sub(page.max(1)), page);
        let header = Row::new(["Status", "Priority", "Task", "Claimed by"])
            .style(Style::default().add_modifier(Modifier::BOLD));
        let table = Table::new(
            rows.into_iter().skip(self.scroll.offset).take(page),
            [
                Constraint::Length(11),
                Constraint::Length(8),
                Constraint::Fill(5),
                Constraint::Fill(2),
            ],
        )
        .header(header)
        .block(board_block(format!("{title}{}", self.scroll.title())));
        frame.render_widget(table, area);
    }
}

impl Snapshot {
    fn projects(&self) -> BTreeMap<&str, Vec<&Task>> {
        let mut projects = BTreeMap::<_, Vec<_>>::new();
        for task in &self.tasks {
            projects
                .entry(task.project.as_str())
                .or_default()
                .push(task.as_ref());
        }
        projects
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Ready,
    Launching,
    Started,
    Joined,
    Offered,
    Delivered,
    Claimed,
    Blocked,
    LaunchFailed,
    Closed,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Launching => "Launching",
            Self::Started => "Started",
            Self::Joined => "Joined",
            Self::Offered => "Offered",
            Self::Delivered => "Delivered",
            Self::Claimed => "Claimed",
            Self::LaunchFailed => "Launch err",
            Self::Blocked => "Blocked",
            Self::Closed => "Closed",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Ready => Color::Green,
            Self::Launching
            | Self::Started
            | Self::Joined
            | Self::Offered
            | Self::Delivered
            | Self::Claimed => Color::Cyan,
            Self::LaunchFailed => Color::Red,
            Self::Blocked => Color::Yellow,
            Self::Closed => Color::Gray,
        }
    }
}

fn board_block(title: impl Into<ratatui::text::Line<'static>>) -> Block<'static> {
    Block::default().title(title).borders(Borders::ALL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use marshal_entities::{
        DispatchAttemptId, DispatchHarness, SessionId, TaskBlockerId, TaskClaimId, TaskId,
    };
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn task_board_groups_projects_and_shows_live_task_states() {
        fn task(id: &str, project: &str, title: &str, status: TaskStatus) -> Arc<Task> {
            Arc::new(Task {
                id: TaskId(Arc::from(id)),
                project: project.into(),
                title: title.into(),
                body: String::new(),
                priority: TaskPriority::P2,
                labels: Vec::new(),
                status,
                created: 0,
            })
        }
        let open = TaskStatus::Open;
        let snapshot = Snapshot {
            tasks: vec![
                task("a", "forge/team/alpha", "Prepare worker", open.clone()),
                task("b", "forge/team/alpha", "Dispatch task", open.clone()),
                task("c", "forge/team/beta", "Review launch", open.clone()),
                task("e", "forge/team/beta", "Staff task", open),
                task(
                    "d",
                    "forge/team/beta",
                    "Register host",
                    TaskStatus::Closed { commit: None },
                ),
            ],
            blockers: vec![Arc::new(TaskBlocker {
                id: TaskBlockerId(Arc::from("b/a")),
                task_id: TaskId(Arc::from("b")),
                blocker_id: TaskId(Arc::from("a")),
            })],
            dispatches: vec![Arc::new(DispatchAttempt {
                id: DispatchAttemptId(Arc::from("e")),
                task_id: TaskId(Arc::from("e")),
                launch_id: "launch-1".into(),
                host_id: "host-1".into(),
                harness: DispatchHarness::Codex,
                workspace: "/repo".into(),
                state: DispatchState::Starting,
                created: 0,
            })],
            claims: vec![Arc::new(TaskClaim {
                id: TaskClaimId(Arc::from("c")),
                task_id: TaskId(Arc::from("c")),
                session_id: SessionId(Arc::from("worker-1")),
                lease_id: "test".into(),
                expires: i64::MAX,
            })],
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        let mut view = View::default();
        let mut render = |view: &mut View| {
            terminal
                .draw(|frame| view.draw_rows(&snapshot, frame.area(), frame))
                .unwrap();
            (0..14)
                .map(|row| {
                    (0..100)
                        .map(|col| terminal.backend().buffer()[(col, row)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        let lines = render(&mut view);
        for (state, title) in [
            ("Ready", "Prepare worker"),
            ("Blocked", "Dispatch task"),
            ("Claimed", "Review launch"),
            ("Closed", "Register host"),
            ("Launching", "Staff task"),
        ] {
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains(state) && line.contains(title))
            );
        }
        view.project = Some("forge/team/beta".into());
        let lines = render(&mut view);
        assert!(lines.iter().any(|line| line.contains("forge/team/beta")));
        assert!(!lines.iter().any(|line| line.contains("forge/team/alpha")));
    }
}
