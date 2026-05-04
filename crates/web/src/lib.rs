//! Leptos client for claude-coord. Connects to the daemon over WebSocket via
//! myko-leptos, subscribes to the live `Session` and `Message` queries, and
//! renders a passive operator dashboard.
//!
//! No `Session` entity is created — like the TUI, the web UI is a watcher,
//! not a participant.

use entities::{GetAllMessages, GetAllSessions, SetSessionRole, SessionId};
use leptos::prelude::*;
use leptos_meta::{Title, provide_meta_context};
use std::sync::Arc;

const DEFAULT_ADDRESS: &str = "localhost:6155";

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    // Connect immediately. `provide_myko` no-ops on non-wasm targets but in
    // the browser it constructs the MykoClient and stashes it in context.
    myko_leptos::provide_myko(DEFAULT_ADDRESS);

    let connected = myko_leptos::use_connection_status();
    let sessions = myko_leptos::live_query::<GetAllSessions>(GetAllSessions {});
    let messages = myko_leptos::live_query::<GetAllMessages>(GetAllMessages {});

    view! {
        <Title text="claude-coord"/>
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
            <strong>"claude-coord"</strong>
            <span class="text-muted mono">{address}</span>
            <span class="flex-1"/>
            <span class="text-subtle">
                {move || if connected.get() { "connected" } else { "disconnected" }}
            </span>
        </header>
    }
}

#[component]
fn SessionsCard(sessions: ReadSignal<Vec<std::sync::Arc<entities::Session>>>) -> impl IntoView {
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
                            <th>"role"</th>
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
                            <tr>
                                <td><strong>{s.nickname.clone()}</strong></td>
                                <td class="mono text-muted">{short_id(&s.id.0)}</td>
                                <td class="role-cell">
                                    <RoleSelect
                                        session_id=s.id.clone()
                                        current=s.role.clone()
                                    />
                                </td>
                                <td class="mono text-subtle truncate">{s.cwd.clone()}</td>
                                <td class="mono text-muted">{s.git_branch.clone().unwrap_or_else(|| "—".into())}</td>
                                <td class="text-subtle">{s.current_task.clone().unwrap_or_else(|| "—".into())}</td>
                            </tr>
                        </For>
                    </tbody>
                </table>
            </Show>
        </section>
    }
}

/// Roles offered in the session dropdown. The empty string means "clear".
const ROLE_OPTIONS: &[(&str, &str)] = &[
    ("", "—"),
    ("worker", "worker"),
    ("task_distributor", "task_distributor"),
    ("communicator", "communicator"),
];

#[component]
fn RoleSelect(session_id: SessionId, current: Option<String>) -> impl IntoView {
    let current_value = current.clone().unwrap_or_default();
    let id_for_change = session_id.clone();

    let on_change = move |ev: leptos::ev::Event| {
        let value = leptos::prelude::event_target_value(&ev);
        let role = if value.is_empty() {
            None
        } else {
            Some(Arc::<str>::from(value.as_str()))
        };
        myko_leptos::send_command::<SetSessionRole, ()>(SetSessionRole {
            id: id_for_change.clone(),
            role,
        });
    };

    view! {
        <select class="role-select" on:change=on_change prop:value=current_value.clone()>
            {ROLE_OPTIONS.iter().map(|(value, label)| {
                let selected = *value == current_value;
                view! {
                    <option value=value.to_string() selected=selected>{label.to_string()}</option>
                }
            }).collect_view()}
        </select>
    }
}

#[component]
fn MessagesCard(messages: ReadSignal<Vec<std::sync::Arc<entities::Message>>>) -> impl IntoView {
    let sorted = Memo::new(move |_| {
        let mut v = messages.get();
        v.sort_by_key(|m| std::cmp::Reverse(m.sent_at));
        v.truncate(100);
        v
    });

    view! {
        <section class="card">
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
    id.as_ref()
        .chars()
        .take(8)
        .collect()
}
