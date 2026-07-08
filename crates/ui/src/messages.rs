//! Messages — the live marshal message bus.
//!
//! A reverse-chronological feed of every message: direct sends (peer → peer)
//! and broadcasts (peer → #room), both nickname-labelled. A filter bar scopes
//! the feed by kind (all / direct / broadcast) and free text, and — the key
//! move — it honours the global `Focus`: select a session in the roster (or a
//! room in the rooms pane) and the feed collapses to just that subject's
//! traffic, with a clearable chip. That's what turns five panes into one lens.

use leptos::prelude::*;
use marshal_entities::{GetAllMessages, GetAllRooms, Message};
use pulse_leptos_ui::{EmptyState, SearchField};
use std::collections::HashMap;
use std::sync::Arc;

use crate::nick::{self};
use crate::time::{self};
use crate::use_focus;

/// Newest-first, capped so a long-lived daemon's history doesn't blow the DOM.
const FEED_CAP: usize = 250;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    All,
    Direct,
    Broadcast,
}

#[component]
pub fn MessagesPanel() -> impl IntoView {
    let (messages, loaded) = myko_leptos::live_query_loaded::<GetAllMessages>(|| GetAllMessages {});
    let rooms = myko_leptos::live_query::<GetAllRooms>(|| GetAllRooms {});
    let now = time::use_now();
    let nick = nick::use_nicknames();
    let focus = use_focus();

    let kind = RwSignal::new(Kind::All);
    let search = RwSignal::new(String::new());

    // room id -> display name, so broadcasts read as `#everyone`.
    let room_names = Memo::new(move |_| {
        rooms
            .get()
            .iter()
            .map(|r| (r.id.0.to_string(), r.name.clone()))
            .collect::<HashMap<String, String>>()
    });

    let visible = Memo::new(move |_| {
        let f_session = focus.session.get();
        let f_room = focus.room.get();
        let k = kind.get();
        let q = search.get().to_lowercase();
        let names = room_names.get();
        let mut v: Vec<Arc<Message>> = messages
            .get()
            .into_iter()
            .filter(|m| {
                if let Some(sid) = &f_session {
                    let hit = m.from_session_id.0.as_ref() == sid
                        || m.to_session_id
                            .as_ref()
                            .map(|s| s.0.as_ref() == sid)
                            .unwrap_or(false);
                    if !hit {
                        return false;
                    }
                }
                if let Some(rid) = &f_room {
                    if !m
                        .to_room_id
                        .as_ref()
                        .map(|r| r.0.as_ref() == rid)
                        .unwrap_or(false)
                    {
                        return false;
                    }
                }
                match k {
                    Kind::Direct if m.to_session_id.is_none() => return false,
                    Kind::Broadcast if m.to_room_id.is_none() => return false,
                    _ => {}
                }
                if !q.is_empty() {
                    let from = nick.of(m.from_session_id.0.as_ref());
                    let to = m
                        .to_session_id
                        .as_ref()
                        .map(|s| nick.of(s.0.as_ref()))
                        .or_else(|| {
                            m.to_room_id
                                .as_ref()
                                .map(|r| names.get(&r.0.to_string()).cloned().unwrap_or_default())
                        })
                        .unwrap_or_default();
                    if !format!("{} {from} {to}", m.body)
                        .to_lowercase()
                        .contains(&q)
                    {
                        return false;
                    }
                }
                true
            })
            .collect();
        v.sort_by_key(|m| std::cmp::Reverse(m.sent_at));
        v.truncate(FEED_CAP);
        v
    });

    view! {
        <div class="panel-head">
            <h2>"messages"</h2>
            <span class="sub">{move || format!("{} shown", visible.get().len())}</span>
        </div>

        // ── stat band (over ALL messages, not the filtered view) ───────────────
        {move || {
            let now_ms = now.ms();
            let all = messages.get();
            let total = all.len();
            let direct = all.iter().filter(|m| m.to_session_id.is_some()).count();
            let broadcast = all.iter().filter(|m| m.to_room_id.is_some()).count();
            let recent = all.iter().filter(|m| time::age_secs(m.sent_at, now_ms) < 300).count();
            view! {
                <div class="statband">
                    <div class="stat"><span class="n">{total}</span><span class="l">"total"</span></div>
                    <div class="stat"><span class="n">{direct}</span><span class="l">"direct"</span></div>
                    <div class="stat"><span class="n">{broadcast}</span><span class="l">"broadcast"</span></div>
                    <div class="stat ok"><span class="n">{recent}</span><span class="l">"last 5m"</span></div>
                </div>
            }
        }}

        // ── filter bar: kind toggle · search · active-focus chip ────────────────
        <div class="mfilter">
            <div class="seg">
                <button class=move || if kind.get() == Kind::All { "on" } else { "" } on:click=move |_| kind.set(Kind::All)>"all"</button>
                <button class=move || if kind.get() == Kind::Direct { "on" } else { "" } on:click=move |_| kind.set(Kind::Direct)>"direct"</button>
                <button class=move || if kind.get() == Kind::Broadcast { "on" } else { "" } on:click=move |_| kind.set(Kind::Broadcast)>"broadcast"</button>
            </div>
            <div class="mfilter-search">
                <SearchField value=search placeholder="search messages" />
            </div>
            {move || {
                if let Some(sid) = focus.session.get() {
                    let n = nick.of(&sid);
                    Some(view! {
                        <button class="focus-chip" on:click=move |_| focus.clear()>
                            "focus: " <span class="nick">{n}</span> <span class="x">"✕"</span>
                        </button>
                    }.into_any())
                } else if let Some(rid) = focus.room.get() {
                    let name = room_names.get().get(&rid).cloned().unwrap_or_else(|| nick::short_id(&rid));
                    Some(view! {
                        <button class="focus-chip" on:click=move |_| focus.clear()>
                            "focus: " <span class="to room">"#" {name}</span> <span class="x">"✕"</span>
                        </button>
                    }.into_any())
                } else {
                    None
                }
            }}
        </div>

        // ── the feed ───────────────────────────────────────────────────────────
        {move || {
            let msgs = visible.get();
            if msgs.is_empty() {
                let msg = if messages.get().len() > 0 {
                    "No messages match the filter."
                } else if loaded.get() {
                    "No messages yet. Sends and broadcasts stream in here live."
                } else {
                    "Connecting to the marshal daemon…"
                };
                return view! { <EmptyState message=msg.to_string() /> }.into_any();
            }
            let names = room_names.get();
            view! {
                <ul class="msg-feed">
                    {msgs.into_iter().map(|m| {
                        let from = nick.of(m.from_session_id.0.as_ref());
                        let (to_label, to_class) = if let Some(sid) = m.to_session_id.as_ref() {
                            (nick.of(sid.0.as_ref()), "to")
                        } else if let Some(rid) = m.to_room_id.as_ref() {
                            let key = rid.0.to_string();
                            let name = names.get(&key).cloned().unwrap_or_else(|| nick::short_id(&key));
                            (format!("#{name}"), "to room")
                        } else {
                            ("—".to_string(), "to dim")
                        };
                        let sent_at = m.sent_at;
                        let abs = time::abs_time(sent_at);
                        let body = m.body.clone();
                        view! {
                            <li class="msg">
                                <div class="msg-meta">
                                    <span class=move || format!("msg-when seen-{}", time::Freshness::from_age_secs(time::age_secs(sent_at, now.ms())).class()) title=abs.clone()>
                                        {move || time::humanize_age(time::age_secs(sent_at, now.ms()))}
                                    </span>
                                    <span class="nick">{from}</span>
                                    <span class="arrow">"→"</span>
                                    <span class=to_class>{to_label}</span>
                                </div>
                                <div class="body">{body}</div>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            }.into_any()
        }}

        <style>{MESSAGES_CSS}</style>
    }
}

const MESSAGES_CSS: &str = r#"
.mfilter { display: flex; align-items: center; gap: 10px; margin-bottom: 12px; flex-wrap: wrap; }
.mfilter .seg { display: inline-flex; border: 1px solid var(--color-border); border-radius: var(--radius-sm); overflow: hidden; }
.mfilter .seg button { background: transparent; color: var(--color-text-secondary); border: none; border-right: 1px solid var(--color-border); padding: 5px 12px; font-family: var(--font-mono); font-size: 11px; cursor: pointer; }
.mfilter .seg button:last-child { border-right: none; }
.mfilter .seg button.on { background: var(--color-base-300); color: var(--color-text-primary); }
.mfilter-search { flex: 1 1 180px; max-width: 320px; }
.focus-chip { display: inline-flex; align-items: baseline; gap: 6px; appearance: none; cursor: pointer; border: 1px solid var(--color-primary); background: color-mix(in oklch, var(--color-primary) 12%, transparent); color: var(--color-text-primary); font-family: var(--font-mono); font-size: 11px; padding: 4px 9px; border-radius: var(--radius-pill, 999px); }
.focus-chip .x { color: var(--color-text-secondary); }
.focus-chip:hover .x { color: var(--color-error); }

.msg-feed { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; }
/* log block: a compact meta line (age · sender → recipient), then the body on
   its own line spanning the FULL pane width — no indent, no wasted left gutter
   on wrapped lines. */
.msg { display: block; padding: 5px 8px; border-bottom: 1px solid var(--color-border); font-family: var(--font-mono); font-size: 12px; }
.msg:hover { background: var(--color-base-300); }
.msg .msg-meta { display: flex; align-items: baseline; gap: 7px; }
.msg .msg-meta .msg-when { font-size: 10px; opacity: 0.75; }
.msg .msg-meta .arrow { color: var(--color-text-secondary); }
.msg .to { color: var(--color-success); font-weight: 600; }
.msg .to.room { color: var(--color-info); }
.msg .to.dim { color: var(--color-text-secondary); }
.msg .body { display: block; margin-top: 2px; color: var(--color-text-primary); white-space: normal; word-break: break-word; line-height: 1.45; }
"#;
