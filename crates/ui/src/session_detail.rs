//! Session detail — a drill-down on the session selected in the Roster.
//!
//! Reads the shared `Selected` signal (set by a Roster row click) and renders
//! everything the roster row can't fit: full identity + host, lifecycle
//! timestamps with live freshness, the rooms the session belongs to, and its
//! most recent traffic (to and from), all nickname-labelled.

use leptos::prelude::*;
use marshal_entities::{
    GetAllMessages, GetAllRoomMembers, GetAllRooms, GetAllSessions, Message, Session,
};
use pulse_leptos_ui::EmptyState;
use std::collections::HashMap;
use std::sync::Arc;

use crate::nick::{self};
use crate::time::{self, Now};
use crate::use_focus;

/// Recent traffic rows shown for the selected session.
const TRAFFIC_CAP: usize = 20;

#[component]
pub fn SessionDetail() -> impl IntoView {
    let focus = use_focus();
    let sessions = myko_leptos::live_query::<GetAllSessions>(|| GetAllSessions {});
    let messages = myko_leptos::live_query::<GetAllMessages>(|| GetAllMessages {});
    let members = myko_leptos::live_query::<GetAllRoomMembers>(|| GetAllRoomMembers {});
    let rooms = myko_leptos::live_query::<GetAllRooms>(|| GetAllRooms {});
    let now = time::use_now();
    let nick = nick::use_nicknames();

    let room_names = Memo::new(move |_| {
        rooms
            .get()
            .iter()
            .map(|r| (r.id.0.to_string(), r.name.clone()))
            .collect::<HashMap<String, String>>()
    });

    // The DATA body — depends on selection + entity queries, but NOT on the
    // clock, so a freshness tick doesn't re-render it (ages live in inner
    // reactive spans below). The reply box is mounted separately, outside this
    // closure, so it survives data churn.
    let body = move || {
        let Some(sid) = focus.session.get() else {
            return view! {
                <EmptyState message="Select a session in the Roster to inspect it.".to_string() />
            }
            .into_any();
        };

        let Some(session) = sessions.get().into_iter().find(|s| s.id.0.as_ref() == sid) else {
            return view! {
                <EmptyState message=format!("Session {} is no longer on the roster.", nick::short_id(&sid)) />
            }
            .into_any();
        };

        let handle = nick.of(&sid);
        let short = nick::short_id(&sid);

        let names = room_names.get();
        let my_rooms: Vec<String> = members
            .get()
            .iter()
            .filter(|rm| rm.session_id.0.as_ref() == sid)
            .map(|rm| {
                let key = rm.room_id.0.to_string();
                names.get(&key).cloned().unwrap_or_else(|| nick::short_id(&key))
            })
            .collect();

        let mut traffic: Vec<Arc<Message>> = messages
            .get()
            .into_iter()
            .filter(|m| {
                m.from_session_id.0.as_ref() == sid
                    || m.to_session_id.as_ref().map(|s| s.0.as_ref() == sid).unwrap_or(false)
            })
            .collect();
        traffic.sort_by_key(|m| std::cmp::Reverse(m.sent_at));
        traffic.truncate(TRAFFIC_CAP);

        view! {
            <div class="sd">
                <div class="sd-hero">
                    <span class="sd-nick">{handle}</span>
                    <span class="sid">{short}</span>
                </div>

                <div class="sd-kv">
                    {kv("operator", session.operator.clone().unwrap_or_else(|| "—".into()))}
                    {kv("host", host_line(&session))}
                    {kv("project", session.project.clone().unwrap_or_else(|| "—".into()))}
                    {kv("branch", session.git_branch.clone().unwrap_or_else(|| "—".into()))}
                    {kv("cwd", session.cwd.clone())}
                    {kv("pid", session.pid.to_string())}
                    {kv("status", session.current_task.clone().unwrap_or_else(|| "—".into()))}
                    {kv_age("connected", session.connected_at, now)}
                    {kv_fresh("last active", session.last_activity_at, now)}
                    {kv_tool("last tool", session.last_tool.clone(), session.last_tool_at, now)}
                    {kv("live channels", channels_line(session.channels_enabled))}
                </div>

                <div class="sd-section">
                    <div class="sd-section-h">"rooms"</div>
                    {if my_rooms.is_empty() {
                        view! { <span class="sd-empty">"in no rooms"</span> }.into_any()
                    } else {
                        view! {
                            <div class="sd-chips">
                                {my_rooms.into_iter().map(|r| view! {
                                    <span class="chip">"#" {r}</span>
                                }).collect_view()}
                            </div>
                        }.into_any()
                    }}
                </div>

                <div class="sd-section">
                    <div class="sd-section-h">"recent traffic"</div>
                    {if traffic.is_empty() {
                        view! { <span class="sd-empty">"no messages"</span> }.into_any()
                    } else {
                        let sid_owned = sid.clone();
                        view! {
                            <ul class="sd-traffic">
                                {traffic.into_iter().map(|m| {
                                    let outbound = m.from_session_id.0.as_ref() == sid_owned;
                                    let other = if outbound {
                                        m.to_session_id.as_ref().map(|s| nick.of(s.0.as_ref()))
                                            .or_else(|| m.to_room_id.as_ref().map(|r| {
                                                format!("#{}", names.get(&r.0.to_string()).cloned()
                                                    .unwrap_or_else(|| nick::short_id(r.0.as_ref())))
                                            }))
                                            .unwrap_or_else(|| "—".into())
                                    } else {
                                        nick.of(m.from_session_id.0.as_ref())
                                    };
                                    let sent_at = m.sent_at;
                                    let dir = if outbound { "→" } else { "←" };
                                    let dir_cls = if outbound { "sd-dir out" } else { "sd-dir in" };
                                    let body = m.body.clone();
                                    view! {
                                        <li class="sd-msg">
                                            <span class=dir_cls>{dir}</span>
                                            <span class="nick">{other}</span>
                                            <span class="sd-when">
                                                {move || format!("{} ago", time::humanize_age(time::age_secs(sent_at, now.ms())))}
                                            </span>
                                            <span class="sd-body">{body}</span>
                                        </li>
                                    }
                                }).collect_view()}
                            </ul>
                        }.into_any()
                    }}
                </div>
            </div>
        }
        .into_any()
    };

    view! {
        <div class="panel-head"><h2>"session"</h2></div>
        {body}
        // Reply box: mounted outside the data closure so typing survives data
        // churn; only re-created when the selection changes.
        {move || focus.session.get().map(|sid| view! { <crate::console::ReplyBox target=sid /> })}
        <style>{DETAIL_CSS}</style>
    }
}

fn kv(k: &'static str, v: String) -> impl IntoView {
    view! {
        <div class="kv"><span class="k">{k}</span><span class="v">{v}</span></div>
    }
}

/// A key/value row whose value is a live age (re-renders each second via the
/// inner closure, not the whole pane).
fn kv_age(k: &'static str, ts: i64, now: Now) -> impl IntoView {
    view! {
        <div class="kv"><span class="k">{k}</span>
            <span class="v">
                {move || format!("{} ago", time::humanize_age(time::age_secs(ts, now.ms())))}
            </span>
        </div>
    }
}

/// A key/value row whose value is a live freshness-coloured age (last active).
fn kv_fresh(k: &'static str, ts: Option<i64>, now: Now) -> AnyView {
    match ts {
        Some(t) => view! {
            <div class="kv"><span class="k">{k}</span>
                <span class=move || format!("v seen-{}", time::Freshness::from_age_secs(time::age_secs(t, now.ms())).class())>
                    {move || format!("{} ago", time::humanize_age(time::age_secs(t, now.ms())))}
                </span>
            </div>
        }
        .into_any(),
        None => kv(k, "—".to_string()).into_any(),
    }
}

/// The last-tool row: name plus a live age.
fn kv_tool(k: &'static str, name: Option<String>, at: Option<i64>, now: Now) -> AnyView {
    match name {
        Some(n) => view! {
            <div class="kv"><span class="k">{k}</span>
                <span class="v">
                    {n}
                    {at.map(|t| view! {
                        <span class="sd-tool-age">
                            {move || format!("  ·  {} ago", time::humanize_age(time::age_secs(t, now.ms())))}
                        </span>
                    })}
                </span>
            </div>
        }
        .into_any(),
        None => kv(k, "—".to_string()).into_any(),
    }
}

fn host_line(s: &Session) -> String {
    match &s.host {
        Some(h) => format!("{}  ·  {} / {}", h.name, h.os, h.arch),
        None => "—".to_string(),
    }
}

fn channels_line(v: Option<bool>) -> String {
    match v {
        Some(true) => "on".to_string(),
        Some(false) => "off".to_string(),
        None => "unknown".to_string(),
    }
}

const DETAIL_CSS: &str = r#"
.sd { display: flex; flex-direction: column; gap: 18px; }
.sd-hero { display: flex; align-items: baseline; gap: 10px; }
.sd-nick { font-family: var(--font-mono); font-weight: 700; font-size: 20px; color: var(--color-primary); }
.sd-kv { display: grid; grid-template-columns: max-content 1fr; gap: 6px 16px; }
.sd-kv .kv { display: contents; }
.sd-kv .k { font-size: 10px; text-transform: uppercase; letter-spacing: 0.07em; color: var(--color-text-secondary); font-family: var(--font-mono); align-self: baseline; padding-top: 2px; }
.sd-kv .v { font-family: var(--font-mono); font-size: 12px; color: var(--color-text-primary); word-break: break-word; }
.sd-tool-age { color: var(--color-text-secondary); }
.sd-section { display: flex; flex-direction: column; gap: 8px; border-top: 1px solid var(--color-border); padding-top: 14px; }
.sd-section-h { font-size: 10px; text-transform: uppercase; letter-spacing: 0.08em; color: var(--color-text-secondary); font-weight: 700; }
.sd-empty { font-family: var(--font-mono); font-size: 11px; color: var(--color-text-secondary); opacity: 0.6; }
.sd-chips { display: flex; flex-wrap: wrap; gap: 6px; }
.sd-chips .chip { font-family: var(--font-mono); font-size: 11px; padding: 2px 8px; border: 1px solid var(--color-border); border-radius: var(--radius-pill, 999px); background: var(--color-base-200); color: var(--color-info); }
.sd-traffic { list-style: none; display: flex; flex-direction: column; gap: 2px; }
.sd-msg { display: grid; grid-template-columns: 14px minmax(90px, max-content) 56px 1fr; gap: 8px; align-items: baseline; padding: 4px 4px; border-bottom: 1px solid var(--color-border); font-family: var(--font-mono); font-size: 11.5px; }
.sd-dir { font-weight: 700; }
.sd-dir.out { color: var(--color-success); }
.sd-dir.in { color: var(--color-info); }
.sd-when { text-align: right; font-size: 10px; color: var(--color-text-secondary); }
.sd-body { color: var(--color-text-primary); white-space: normal; word-break: break-word; line-height: 1.4; }
"#;
