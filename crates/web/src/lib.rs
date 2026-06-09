//! Leptos client for marshal. Connects to the daemon over WebSocket via
//! myko-leptos, subscribes to the live `Session` and `Message` queries, and
//! renders a passive operator dashboard.
//!
//! No `Session` entity is created — like the TUI, the web UI is a watcher,
//! not a participant.

use leptos::prelude::*;
use leptos_meta::{Title, provide_meta_context};
use marshal_entities::{GetAllMessages, GetAllSessions};

const DEFAULT_ADDRESS: &str = "localhost:6155";

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    // Connect immediately. `provide_myko` no-ops on non-wasm targets but in
    // the browser it constructs the MykoClient and stashes it in context.
    myko_leptos::provide_myko(DEFAULT_ADDRESS);

    let connected = myko_leptos::use_connection_status();
    // myko-leptos 4.17: live_query takes a closure returning the query.
    let sessions = myko_leptos::live_query(|| GetAllSessions {});
    let messages = myko_leptos::live_query(|| GetAllMessages {});

    view! {
        <Title text="marshal"/>
        <div class="app">
            <Header connected=connected.into() address=DEFAULT_ADDRESS />
            <SessionsCard sessions=sessions />
            <MessagesCard messages=messages />
        </div>
    }
}

#[component]
fn Header(connected: Signal<bool>, address: &'static str) -> impl IntoView {
    view! {
        <header class="status-bar">
            <div class=move || {
                if connected.get() { "status-dot connected" } else { "status-dot disconnected" }
            }/>
            <strong>"marshal"</strong>
            <span class="text-muted mono">{address}</span>
            <span class="flex-1"/>
            <span class="text-subtle">
                {move || if connected.get() { "connected" } else { "disconnected" }}
            </span>
        </header>
    }
}

#[component]
fn SessionsCard(
    sessions: ReadSignal<Vec<std::sync::Arc<marshal_entities::Session>>>,
) -> impl IntoView {
    view! {
        <section class="card">
            <h2>"Sessions " <span class="text-muted">"(" {move || sessions.get().len()} ")"</span></h2>
            <Show
                when=move || !sessions.get().is_empty()
                fallback=|| view! { <p class="empty">"No sessions connected."</p> }
            >
                <table class="data">
                    <thead>
                        <tr>
                            <th>"nick"</th>
                            <th>"id"</th>
                            <th>"cwd"</th>
                            <th>"branch"</th>
                            <th>"status"</th>
                        </tr>
                    </thead>
                    <tbody>
                        <For
                            each=move || sessions.get()
                            key=|s| s.id.0.to_string()
                            let:s
                        >
                            <SessionRow session=s />
                        </For>
                    </tbody>
                </table>
            </Show>
        </section>
    }
}

/// One session row. The status column surfaces `current_task` (the
/// free-form text peers set via `set_status`) plus liveness fields
/// (last_activity_at, last_tool, last_tool_at) the shim publishes
/// every ~5s, so an operator can spot a stuck peer vs an active one
/// at a glance.
#[component]
fn SessionRow(session: std::sync::Arc<marshal_entities::Session>) -> impl IntoView {
    let last_activity_at = session.last_activity_at;
    let last_tool = session.last_tool.clone();
    let last_tool_at = session.last_tool_at;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let activity_class = freshness_class(last_activity_at, now_ms);
    let activity_age = last_activity_at
        .map(|ts| format_age(now_ms - ts))
        .unwrap_or_else(|| "—".into());
    let tool_age = last_tool_at
        .map(|ts| format_age(now_ms - ts))
        .unwrap_or_default();
    let current_task = session.current_task.clone();
    let status_view = move || -> AnyView {
        let task_line: AnyView = match &current_task {
            Some(text) => view! {
                <div class="liveness-task">
                    <strong>{text.clone()}</strong>
                </div>
            }
            .into_any(),
            None => view! {
                <div class="liveness-task text-muted">"— no status —"</div>
            }
            .into_any(),
        };
        let tool_line: AnyView = match &last_tool {
            Some(name) => view! {
                <div class="liveness-tool">
                    <span class="mono text-subtle">{name.clone()}</span>
                    " "
                    <span class="text-muted">{tool_age.clone()}</span>
                </div>
            }
            .into_any(),
            None => view! {
                <div class="liveness-tool text-muted">"no tool calls yet"</div>
            }
            .into_any(),
        };
        view! {
            {task_line}
            {tool_line}
            <div class={format!("liveness-age {activity_class}")}>
                "active " {activity_age.clone()}
            </div>
        }
        .into_any()
    };
    view! {
        <tr>
            <td><strong>{session.nickname.clone()}</strong></td>
            <td class="mono text-muted">{short_id(&session.id.0)}</td>
            <td class="mono text-subtle truncate">{session.cwd.clone()}</td>
            <td class="mono text-muted">{session.git_branch.clone().unwrap_or_else(|| "—".into())}</td>
            <td class="text-subtle">{status_view}</td>
        </tr>
    }
}

/// Soft-stale at 60s, hard-stale at 5min. Returns a CSS class name
/// the row applies to its activity-age span; styling drains the
/// color from yellow → red as the session drifts further from "alive".
/// `None` (no recorded activity yet) renders as `unknown` so a
/// brand-new session's row doesn't immediately panic the operator.
fn freshness_class(last_activity_at: Option<i64>, now_ms: i64) -> &'static str {
    let Some(ts) = last_activity_at else {
        return "freshness-unknown";
    };
    let age_ms = now_ms.saturating_sub(ts);
    if age_ms < 60_000 {
        "freshness-fresh"
    } else if age_ms < 300_000 {
        "freshness-stale-soft"
    } else {
        "freshness-stale-hard"
    }
}

/// Compact "5s ago" / "2m ago" / "1.5h ago" formatter for the
/// liveness panel. Negative offsets (clock drift between client and
/// server) clamp to "just now" rather than printing a confusing
/// negative duration.
fn format_age(ms: i64) -> String {
    if ms < 1_000 {
        return "just now".into();
    }
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[component]
fn MessagesCard(
    messages: ReadSignal<Vec<std::sync::Arc<marshal_entities::Message>>>,
) -> impl IntoView {
    let sorted = Memo::new(move |_| {
        let mut v = messages.get();
        v.sort_by_key(|m| std::cmp::Reverse(m.sent_at));
        v.truncate(100);
        v
    });

    view! {
        <section class="card messages-card">
            <h2>"Messages " <span class="text-muted">"(" {move || sorted.get().len()} ")"</span></h2>
            <Show
                when=move || !sorted.get().is_empty()
                fallback=|| view! { <p class="empty">"No messages yet."</p> }
            >
                <ul class="messages">
                    <For
                        each=move || sorted.get()
                        key=|m| m.id.0.to_string()
                        let:m
                    >
                        <li class="msg">
                            <span class="from"><strong>{m.from_nick.clone()}</strong></span>
                            <span class="arrow">"→"</span>
                            <span class="to"><strong>{m.to_nick.clone()}</strong></span>
                            <span class="body">{m.body.clone()}</span>
                        </li>
                    </For>
                </ul>
            </Show>
        </section>
    }
}

fn short_id(id: &std::sync::Arc<str>) -> String {
    id.as_ref().chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_unknown_when_no_activity() {
        assert_eq!(freshness_class(None, 1_000_000), "freshness-unknown");
    }

    #[test]
    fn freshness_fresh_under_one_minute() {
        assert_eq!(
            freshness_class(Some(1_000_000_000), 1_000_030_000),
            "freshness-fresh"
        );
    }

    #[test]
    fn freshness_stale_soft_between_one_and_five_minutes() {
        assert_eq!(
            freshness_class(Some(1_000_000_000), 1_000_000_000 + 120_000),
            "freshness-stale-soft"
        );
    }

    #[test]
    fn freshness_stale_hard_past_five_minutes() {
        assert_eq!(
            freshness_class(Some(1_000_000_000), 1_000_000_000 + 360_000),
            "freshness-stale-hard"
        );
    }

    #[test]
    fn format_age_handles_sub_second() {
        assert_eq!(format_age(0), "just now");
        assert_eq!(format_age(500), "just now");
    }

    #[test]
    fn format_age_seconds_minutes_hours_days() {
        assert_eq!(format_age(5_000), "5s ago");
        assert_eq!(format_age(120_000), "2m ago");
        assert_eq!(format_age(7_200_000), "2h ago");
        assert_eq!(format_age(172_800_000), "2d ago");
    }
}
