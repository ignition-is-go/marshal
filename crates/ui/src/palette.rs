//! ⌘K command palette — fuzzy-jump the focus to any session or room.
//!
//! pulse-leptos-ui's `CommandPalette` navigates via `<a href>` (no router here,
//! and we want to run an *action* on select, not navigate), so we reuse only its
//! `use_command_palette_shortcut` hook (⌘K / Ctrl+K / `/` to toggle, Esc to
//! close) and render a small overlay that sets the global `Focus` on pick.

use leptos::prelude::*;
use marshal_entities::{GetAllRooms, GetAllSessions};
use pulse_leptos_ui::hooks::use_command_palette_shortcut;

use crate::nick::use_nicknames;
use crate::use_focus;

#[derive(Clone, PartialEq)]
enum Target {
    Session(String),
    Room(String),
}

#[component]
pub fn CommandK() -> impl IntoView {
    let open = RwSignal::new(false);
    use_command_palette_shortcut(open);
    let query = RwSignal::new(String::new());
    let input_ref = NodeRef::<leptos::html::Input>::new();

    let sessions = myko_leptos::live_query::<GetAllSessions>(|| GetAllSessions {});
    let rooms = myko_leptos::live_query::<GetAllRooms>(|| GetAllRooms {});
    let nick = use_nicknames();
    let focus = use_focus();

    // Focus the input whenever the palette opens.
    Effect::new(move |_| {
        if open.get()
            && let Some(el) = input_ref.get()
        {
            let _ = el.focus();
        }
    });

    let results = Memo::new(move |_| {
        let q = query.get().to_lowercase();
        let mut out: Vec<(&'static str, String, Option<String>, Target)> = Vec::new();
        for s in sessions.get() {
            let handle = nick.of(s.id.0.as_ref());
            let hay = format!(
                "{handle} {} {}",
                s.operator.clone().unwrap_or_default(),
                s.host.as_ref().map(|h| h.name.clone()).unwrap_or_default(),
            );
            if q.is_empty() || hay.to_lowercase().contains(&q) {
                out.push((
                    "session",
                    handle,
                    s.current_task.clone(),
                    Target::Session(s.id.0.to_string()),
                ));
            }
        }
        for r in rooms.get() {
            if q.is_empty() || r.name.to_lowercase().contains(&q) {
                out.push((
                    "room",
                    format!("#{}", r.name),
                    None,
                    Target::Room(r.id.0.to_string()),
                ));
            }
        }
        out.truncate(60);
        out
    });

    let pick = move |t: Target| {
        match t {
            Target::Session(id) => focus.set_session(id),
            Target::Room(id) => focus.set_room(id),
        }
        query.set(String::new());
        open.set(false);
    };

    view! {
        {move || open.get().then(|| view! {
            <div class="ck-scrim" on:click=move |_| open.set(false)>
                <div class="ck" on:click=move |e| e.stop_propagation()>
                    <input
                        node_ref=input_ref
                        class="ck-input"
                        placeholder="jump to a session or room…"
                        prop:value=move || query.get()
                        on:input=move |e| query.set(event_target_value(&e))
                    />
                    <ul class="ck-list">
                        {move || {
                            let rs = results.get();
                            if rs.is_empty() {
                                return view! { <li class="ck-empty">"no matches"</li> }.into_any();
                            }
                            rs.into_iter().map(|(kind, disp, sub, target)| {
                                view! {
                                    <li class="ck-item" on:click=move |_| pick(target.clone())>
                                        <span class="ck-kind">{kind}</span>
                                        <span class="ck-name">{disp}</span>
                                        {sub.map(|s| view! { <span class="ck-sub">{s}</span> })}
                                    </li>
                                }
                            }).collect_view().into_any()
                        }}
                    </ul>
                    <div class="ck-hint"><span>"esc"</span>"to close · ⌘K to reopen"</div>
                </div>
            </div>
        })}
        <style>{CK_CSS}</style>
    }
}

const CK_CSS: &str = r#"
.ck-scrim { position: fixed; inset: 0; background: rgba(0,0,0,0.5); display: flex; justify-content: center; align-items: flex-start; padding-top: 12vh; z-index: 100; }
.ck { width: min(560px, 90vw); background: var(--color-base-200); border: 1px solid var(--color-border-strong); border-radius: var(--radius-md, 8px); box-shadow: var(--shadow-pane-floating); overflow: hidden; display: flex; flex-direction: column; max-height: 62vh; }
.ck-input { border: none; border-bottom: 1px solid var(--color-border); background: transparent; color: var(--color-text-primary); font-family: var(--font-mono); font-size: 14px; padding: 14px 16px; outline: none; }
.ck-list { list-style: none; margin: 0; padding: 0; overflow-y: auto; }
.ck-item { display: flex; align-items: baseline; gap: 10px; padding: 8px 16px; cursor: pointer; font-family: var(--font-mono); font-size: 12px; border-bottom: 1px solid var(--color-border); }
.ck-item:hover { background: var(--color-base-300); }
.ck-kind { font-size: 9px; text-transform: uppercase; letter-spacing: 0.08em; color: var(--color-text-secondary); width: 52px; flex: 0 0 auto; }
.ck-name { color: var(--color-primary); font-weight: 600; flex: 0 0 auto; }
.ck-sub { color: var(--color-text-secondary); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
.ck-empty { padding: 16px; text-align: center; color: var(--color-text-secondary); font-family: var(--font-mono); font-size: 12px; }
.ck-hint { padding: 8px 16px; border-top: 1px solid var(--color-border); font-family: var(--font-mono); font-size: 10px; color: var(--color-text-secondary); }
.ck-hint span { border: 1px solid var(--color-border); border-radius: 3px; padding: 1px 5px; margin-right: 6px; }
"#;
