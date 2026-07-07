//! Messages — the live marshal message bus.
//!
//! A reverse-chronological feed of every message on the daemon: direct sends
//! (peer → peer) and broadcasts (peer → #room). Both ends are labelled by
//! nickname — the room name is resolved from the live `Room` query so a
//! broadcast reads as `#everyone`, not a slug. A stat band summarises volume.

use leptos::prelude::*;
use marshal_entities::{GetAllMessages, GetAllRooms};
use pulse_leptos_ui::EmptyState;
use std::collections::HashMap;

use crate::nick::{self};
use crate::time::{self};

/// Newest-first, capped so a long-lived daemon's history doesn't blow the DOM.
const FEED_CAP: usize = 250;

#[component]
pub fn MessagesPanel() -> impl IntoView {
    let messages = myko_leptos::live_query::<GetAllMessages>(|| GetAllMessages {});
    let rooms = myko_leptos::live_query::<GetAllRooms>(|| GetAllRooms {});
    let now = time::use_now();
    let nick = nick::use_nicknames();

    // room id -> display name, so broadcasts read as `#everyone`.
    let room_names = Memo::new(move |_| {
        rooms
            .get()
            .iter()
            .map(|r| (r.id.0.to_string(), r.name.clone()))
            .collect::<HashMap<String, String>>()
    });

    let sorted = Memo::new(move |_| {
        let mut v = messages.get();
        v.sort_by_key(|m| std::cmp::Reverse(m.sent_at));
        v.truncate(FEED_CAP);
        v
    });

    view! {
        <div class="panel-head">
            <h2>"messages"</h2>
            <span class="sub">{move || format!("{} shown", sorted.get().len())}</span>
        </div>

        // ── stat band ──────────────────────────────────────────────────────────
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

        // ── the feed — re-renders on new messages, but per-row ages tick via
        //    inner closures so the whole list isn't rebuilt every second ───────
        {move || {
            let msgs = sorted.get();
            if msgs.is_empty() {
                return view! {
                    <EmptyState message="No messages yet. Sends and broadcasts stream in here live.".to_string() />
                }.into_any();
            }
            let names = room_names.get();
            view! {
                <ul class="msg-feed">
                    {msgs.into_iter().map(|m| {
                        let from = nick.of(m.from_session_id.0.as_ref());
                        // exactly one recipient side is set (direct vs broadcast).
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
                        let body = m.body.clone();
                        view! {
                            <li class="msg">
                                <span class=move || format!("msg-when seen-{}", time::Freshness::from_age_secs(time::age_secs(sent_at, now.ms())).class())>
                                    {move || format!("{} ago", time::humanize_age(time::age_secs(sent_at, now.ms())))}
                                </span>
                                <span class="nick">{from}</span>
                                <span class="arrow">"→"</span>
                                <span class=to_class>{to_label}</span>
                                <span class="body">{body}</span>
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
.msg-feed { list-style: none; display: flex; flex-direction: column; }
.msg { display: grid; grid-template-columns: 64px minmax(110px, max-content) 14px minmax(110px, max-content) 1fr; gap: 8px; align-items: baseline; padding: 5px 6px; border-bottom: 1px solid var(--color-border); font-family: var(--font-mono); font-size: 12px; }
.msg:hover { background: var(--color-base-300); }
.msg .msg-when { text-align: right; font-size: 10px; opacity: 0.8; }
.msg .arrow { color: var(--color-text-secondary); }
.msg .to { color: var(--color-success); font-weight: 600; }
.msg .to.room { color: var(--color-info); }
.msg .to.dim { color: var(--color-text-secondary); }
.msg .body { color: var(--color-text-primary); white-space: normal; word-break: break-word; line-height: 1.4; }
"#;
