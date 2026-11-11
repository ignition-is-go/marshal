//! Roster — the live wall of agent sessions on the marshal daemon.
//!
//! A dense, scannable table (not a field of cards): the roster is a list of
//! homogeneous rows, so it reads best as aligned columns grouped by host. The
//! nickname is the primary identity in the leftmost column; a stat band up top
//! answers "who's live?" at a glance. Clicking a row selects that session for
//! the Session detail pane.

use leptos::prelude::*;
use marshal_entities::{GetAllSessions, Session};
use pulse_leptos_ui::{EmptyState, SearchField};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::nick::{self, Nicknames};
use crate::time::{self, Freshness, Now};
use crate::{Focus, use_focus};

/// Effective "last seen" for a session: its most recent activity, or its
/// connect time before it's ever pushed activity.
fn last_seen_ms(s: &Session) -> i64 {
    s.last_activity_at.unwrap_or(s.connected_at)
}

#[component]
pub fn RosterPanel() -> impl IntoView {
    let (sessions, loaded) = myko_leptos::live_query_loaded::<GetAllSessions>(|| GetAllSessions {});
    let now = time::use_now();
    let nick = nick::use_nicknames();
    let focus = use_focus();

    let filter = RwSignal::new(String::new());

    // Filter on session name / nickname / operator / host / project / branch /
    // cwd. Reads the nickname map, so it re-runs as assignments arrive.
    let visible = Memo::new(move |_| {
        let q = filter.get().to_lowercase();
        let mut v: Vec<Arc<Session>> = sessions
            .get()
            .into_iter()
            .filter(|s| {
                if q.is_empty() {
                    return true;
                }
                let hay = format!(
                    "{} {} {} {} {} {} {} {} {}",
                    nick.of(s.id.0.as_ref()),
                    s.session_name.clone().unwrap_or_default(),
                    s.activity.clone().unwrap_or_default(),
                    s.kind.clone().unwrap_or_default(),
                    s.operator.clone().unwrap_or_default(),
                    s.host.as_ref().map(|h| h.name.clone()).unwrap_or_default(),
                    s.project.clone().unwrap_or_default(),
                    s.git_branch.clone().unwrap_or_default(),
                    s.cwd,
                );
                hay.to_lowercase().contains(&q)
            })
            .collect();
        // Most-recently-active first within a host group.
        v.sort_by_key(|s| std::cmp::Reverse(last_seen_ms(s)));
        v
    });

    view! {
        <div class="panel-head">
            <h2>"roster"</h2>
            <span class="sub">{move || format!("{} sessions", visible.get().len())}</span>
        </div>

        // ── stat band — at-a-glance roster health ──────────────────────────────
        {move || {
            let now_ms = now.ms();
            let all = visible.get();
            let total = all.len();
            let bucket = |f: Freshness| all.iter()
                .filter(|s| Freshness::from_age_secs(time::age_secs(last_seen_ms(s), now_ms)) == f)
                .count();
            let active = bucket(Freshness::Fresh);
            let aging = bucket(Freshness::Aging);
            let stale = bucket(Freshness::Stale);
            let ops = all.iter().filter_map(|s| s.operator.clone()).collect::<std::collections::BTreeSet<_>>().len();
            let hosts = all.iter().filter_map(|s| s.host.as_ref().map(|h| h.name.clone())).collect::<std::collections::BTreeSet<_>>().len();
            let dim = |c: usize| if c > 0 { "stat" } else { "stat zero" };
            view! {
                <div class="statband">
                    <div class="stat"><span class="n">{total}</span><span class="l">"sessions"</span></div>
                    <div class="stat ok"><span class="n">{active}</span><span class="l">"active"</span></div>
                    <div class=dim(aging)><span class="n">{aging}</span><span class="l">"aging"</span></div>
                    <div class=dim(stale)><span class="n">{stale}</span><span class="l">"stale"</span></div>
                    <div class="stat"><span class="n">{ops}</span><span class="l">"operators"</span></div>
                    <div class="stat"><span class="n">{hosts}</span><span class="l">"hosts"</span></div>
                </div>
            }
        }}

        <div class="roster-search">
            <SearchField value=filter placeholder="filter by name · nickname · operator · host · project" />
        </div>

        // ── the table, grouped by host ─────────────────────────────────────────
        {move || {
            let all = visible.get();
            if all.is_empty() {
                let msg = if !sessions.get().is_empty() {
                    "No sessions match the filter."
                } else if loaded.get() {
                    "No sessions on the roster. Shims and TUIs appear here as they connect."
                } else {
                    "Connecting to the marshal daemon…"
                };
                return view! { <EmptyState message=msg.to_string() /> }.into_any();
            }
            // Group by host name; sessions without a host fall into "unknown".
            let mut by_host: BTreeMap<String, Vec<Arc<Session>>> = BTreeMap::new();
            for s in all {
                let h = s.host.as_ref().map(|h| h.name.clone()).unwrap_or_else(|| "unknown".to_string());
                by_host.entry(h).or_default().push(s);
            }
            view! {
                <table class="dtable roster-t">
                    <thead>
                        <tr>
                            <th class="c-name">"session"</th>
                            <th>"operator"</th>
                            <th>"project"</th>
                            <th>"branch"</th>
                            <th>"status"</th>
                            <th class="c-num">"seen"</th>
                        </tr>
                    </thead>
                    {by_host.into_iter().map(|(host, members)| {
                        let count = members.len();
                        view! {
                            <tbody>
                                <tr class="grp-row">
                                    <td colspan="6">
                                        <span class="grp">{host}</span>
                                        <span class="grp-n">{format!("{count}")}</span>
                                    </td>
                                </tr>
                                {members.into_iter().map(|s| view! {
                                    <RosterRow session=s now=now nick=nick focus=focus />
                                }).collect_view()}
                            </tbody>
                        }
                    }).collect_view()}
                </table>
            }.into_any()
        }}

        <style>{ROSTER_CSS}</style>
    }
}

#[component]
fn RosterRow(session: Arc<Session>, now: Now, nick: Nicknames, focus: Focus) -> impl IntoView {
    let id = session.id.0.to_string();
    let sid = nick::short_id(&id);
    // The operator's `/rename` name for this session, when set — the human
    // label peers target on; nickname stays the address handle.
    let sname = session.session_name.clone().filter(|s| !s.is_empty());
    // Live turn-state (busy/idle/shell) → a coloured presence dot, and whether
    // this is a background/headless agent.
    let (act_cls, act_title) = activity_indicator(session.activity.as_deref());
    let is_bg = session.kind.as_deref() == Some("bg");
    let seen_ms = last_seen_ms(&session);
    let seen_abs = time::abs_time(seen_ms);

    // Is this row the viewer's own operator console? (shown as "you" when online)
    let console = crate::console::use_console();
    let is_console = session.id.0.as_ref() == console.session_id.as_ref();
    let online = console.online;

    // The identity columns aren't reactive within a row — the parent `For`
    // re-renders the row when the session Arc changes — so precompute them as
    // static text + a "dim when absent" flag.
    let (op_dim, op_str) = opt_cell(&session.operator);
    let (proj_dim, proj_str) = opt_cell(&session.project);
    let branch_str = session.git_branch.clone().unwrap_or_else(|| "—".into());
    let (task_dim, task_str) = opt_status(&session.current_task);
    let task_title = task_str.clone();

    // nickname is reactive (assignment may arrive after the row mounts).
    let nick_id = id.clone();
    let handle = move || nick.of(&nick_id);

    // Row highlight + freshness recede, live each second.
    let sel_id = id.clone();
    let row_class = move || {
        let is_sel = focus
            .session
            .with(|s| s.as_deref() == Some(sel_id.as_str()));
        let stale = Freshness::from_age_secs(time::age_secs(seen_ms, now.ms())) == Freshness::Stale;
        let mut c = String::from("roster-row");
        if is_sel {
            c.push_str(" selected");
        }
        if stale {
            c.push_str(" stale");
        }
        c
    };

    let seen = move || {
        let age = time::age_secs(seen_ms, now.ms());
        let cls = format!("c-num seen-{}", Freshness::from_age_secs(age).class());
        view! { <td class=cls title=seen_abs.clone()>{format!("{} ago", time::humanize_age(age))}</td> }
    };

    let click_id = id.clone();
    view! {
        <tr class=row_class on:click=move |_| focus.toggle_session(click_id.clone())>
            <td class="c-name">
                <span class=act_cls title=act_title></span>
                <span class="nick">{handle}</span>
                <span class="sid">{sid}</span>
                {is_bg.then(|| view! { <span class="kind-badge" title="background / headless agent">"bg"</span> })}
                {move || (is_console && online.get()).then(|| view! { <span class="you-badge">"you"</span> })}
                {sname.map(|n| { let t = n.clone(); view! { <span class="sname" title=t>{n}</span> } })}
            </td>
            <td class=op_dim>{op_str}</td>
            <td class=proj_dim>{proj_str}</td>
            <td class="dim">{branch_str}</td>
            <td class=format!("c-status {task_dim}")>
                <span class="ellip" title=task_title>{task_str}</span>
            </td>
            {seen}
        </tr>
    }
}

/// An optional identity cell → (`class`, `text`): dim + em-dash when absent.
fn opt_cell(v: &Option<String>) -> (&'static str, String) {
    match v {
        Some(s) if !s.is_empty() => ("", s.clone()),
        _ => ("dim", "—".to_string()),
    }
}

/// The status column: dim placeholder when a session has set no status text.
fn opt_status(v: &Option<String>) -> (&'static str, String) {
    match v {
        Some(s) if !s.is_empty() => ("", s.clone()),
        _ => ("dim", "— no status —".to_string()),
    }
}

/// The live turn-state dot → (`class`, hover title). `busy` = actively working
/// (pulses), `shell` = in a shell command, `idle` = awaiting input, absent =
/// unknown (harness without a manifest / older build).
fn activity_indicator(a: Option<&str>) -> (&'static str, &'static str) {
    match a {
        Some("busy") => ("act-dot act-busy", "working"),
        Some("shell") => ("act-dot act-shell", "in shell"),
        Some("idle") => ("act-dot act-idle", "idle"),
        Some(_) => ("act-dot act-idle", "active"),
        None => ("act-dot act-none", "unknown"),
    }
}

const ROSTER_CSS: &str = r#"
.roster-search { margin-bottom: 12px; max-width: 420px; }
.roster-t td.c-name { min-width: 140px; }
.roster-t td.c-name .nick { display: inline-block; }
.roster-t td.c-name .sid { display: inline-block; margin-left: 8px; }
.roster-t td.c-name .sname { display: block; margin-top: 3px; font-size: 11px; color: var(--color-text-secondary); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 220px; }
.roster-t .roster-row { cursor: pointer; }
.roster-t .roster-row.selected td { background: color-mix(in oklch, var(--color-primary) 15%, transparent); }
.roster-t .roster-row.selected .c-name { box-shadow: inset 3px 0 0 var(--color-primary); }
.roster-t .roster-row.stale td { opacity: 0.5; }
.roster-t .roster-row.stale.selected td { opacity: 1; }
/* status is free-form text — cap it and ellipsize so a long status can't blow
   out the column width; the full text is on the cell's title (hover). */
.roster-t td.c-status { max-width: 240px; }
.roster-t td.c-status .ellip { display: block; max-width: 240px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.roster-t .you-badge { margin-left: 8px; font-size: 9px; font-weight: 700; text-transform: uppercase; letter-spacing: 0.06em; color: var(--color-base-100); background: var(--color-primary); padding: 1px 5px; border-radius: 3px; }
/* live turn-state dot: green (pulsing) = working, amber = shell, dim = idle. */
.roster-t td.c-name .act-dot { display: inline-block; width: 7px; height: 7px; border-radius: 50%; margin-right: 7px; vertical-align: middle; }
.roster-t td.c-name .act-busy { background: var(--color-success); animation: act-pulse 1.6s ease-in-out infinite; }
.roster-t td.c-name .act-shell { background: var(--color-warning); }
.roster-t td.c-name .act-idle { background: var(--color-text-secondary); opacity: 0.35; }
.roster-t td.c-name .act-none { background: transparent; box-shadow: inset 0 0 0 1px var(--color-border); }
@keyframes act-pulse { 0%, 100% { opacity: 1; } 50% { opacity: 0.4; } }
.roster-t .kind-badge { margin-left: 8px; font-size: 9px; font-weight: 700; text-transform: uppercase; letter-spacing: 0.05em; color: var(--color-text-secondary); border: 1px solid var(--color-border); padding: 0 4px; border-radius: 3px; }
"#;
