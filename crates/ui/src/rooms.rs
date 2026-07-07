//! Rooms — the routing scopes messages fan out to.
//!
//! Two kinds: identity-anchored auto-rooms (`everyone`, `host:*`, `op:*`,
//! `project:*`, kept in sync by the daemon's saga) and user-created ad-hoc
//! rooms. Each room card shows its kind, description, and current membership by
//! nickname. Auto-rooms sort first.

use leptos::prelude::*;
use marshal_entities::{AutoSource, GetAllRoomMembers, GetAllRooms, Room, RoomKind};
use pulse_leptos_ui::{Badge, BadgeVariant, EmptyState};
use std::collections::HashMap;
use std::sync::Arc;

use crate::nick::{self};

/// (sort-key, badge variant, label) for a room kind. Auto-rooms sort ahead of
/// ad-hoc; within auto, everyone → host → operator → project.
fn kind_meta(kind: &RoomKind) -> (u8, BadgeVariant, &'static str) {
    match kind {
        RoomKind::Auto { source } => match source {
            AutoSource::Everyone => (0, BadgeVariant::Primary, "everyone"),
            AutoSource::Host { .. } => (1, BadgeVariant::Info, "host"),
            AutoSource::Operator { .. } => (2, BadgeVariant::Info, "operator"),
            AutoSource::Project { .. } => (3, BadgeVariant::Info, "project"),
        },
        RoomKind::Adhoc => (4, BadgeVariant::Accent, "adhoc"),
    }
}

#[component]
pub fn RoomsPanel() -> impl IntoView {
    let rooms = myko_leptos::live_query::<GetAllRooms>(|| GetAllRooms {});
    let members = myko_leptos::live_query::<GetAllRoomMembers>(|| GetAllRoomMembers {});
    let nick = nick::use_nicknames();

    // room id -> member session ids.
    let membership = Memo::new(move |_| {
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for rm in members.get() {
            m.entry(rm.room_id.0.to_string())
                .or_default()
                .push(rm.session_id.0.to_string());
        }
        m
    });

    let sorted = Memo::new(move |_| {
        let mut v: Vec<Arc<Room>> = rooms.get();
        v.sort_by(|a, b| {
            let (ka, _, _) = kind_meta(&a.kind);
            let (kb, _, _) = kind_meta(&b.kind);
            ka.cmp(&kb).then_with(|| a.name.cmp(&b.name))
        });
        v
    });

    view! {
        <div class="panel-head">
            <h2>"rooms"</h2>
            <span class="sub">{move || format!("{} rooms", sorted.get().len())}</span>
        </div>

        {move || {
            let all = sorted.get();
            let mem = membership.get();
            let total = all.len();
            let auto = all.iter().filter(|r| matches!(r.kind, RoomKind::Auto { .. })).count();
            let adhoc = total - auto;
            let occupied = all.iter().filter(|r| mem.get(r.id.0.as_ref()).map(|v| !v.is_empty()).unwrap_or(false)).count();
            view! {
                <div class="statband">
                    <div class="stat"><span class="n">{total}</span><span class="l">"rooms"</span></div>
                    <div class="stat"><span class="n">{auto}</span><span class="l">"auto"</span></div>
                    <div class="stat"><span class="n">{adhoc}</span><span class="l">"adhoc"</span></div>
                    <div class="stat"><span class="n">{occupied}</span><span class="l">"occupied"</span></div>
                </div>
            }
        }}

        {move || {
            let all = sorted.get();
            if all.is_empty() {
                return view! {
                    <EmptyState message="No rooms yet. Auto-rooms appear as sessions connect.".to_string() />
                }.into_any();
            }
            let mem = membership.get();
            view! {
                <div class="room-grid">
                    {all.into_iter().map(|r| {
                        let (_, variant, label) = kind_meta(&r.kind);
                        let handles: Vec<String> = mem.get(r.id.0.as_ref())
                            .map(|ids| ids.iter().map(|id| nick.of(id)).collect())
                            .unwrap_or_default();
                        let count = handles.len();
                        let desc = r.description.clone();
                        let name = r.name.clone();
                        view! {
                            <div class="room-card">
                                <div class="room-top">
                                    <span class="room-name">"#" {name}</span>
                                    <Badge variant=variant>{label}</Badge>
                                    <span class="room-n">{format!("{count}")}</span>
                                </div>
                                {desc.map(|d| view! { <div class="room-desc">{d}</div> })}
                                <div class="room-members">
                                    {if handles.is_empty() {
                                        view! { <span class="room-empty">"empty"</span> }.into_any()
                                    } else {
                                        handles.into_iter().map(|h| view! {
                                            <span class="nick chip">{h}</span>
                                        }).collect_view().into_any()
                                    }}
                                </div>
                            </div>
                        }
                    }).collect_view()}
                </div>
            }.into_any()
        }}

        <style>{ROOMS_CSS}</style>
    }
}

const ROOMS_CSS: &str = r#"
.room-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(240px, 1fr)); gap: 10px; }
.room-card { border: 1px solid var(--color-border); border-radius: var(--radius-sm); background: var(--color-base-200); padding: 10px 12px; display: flex; flex-direction: column; gap: 8px; }
.room-top { display: flex; align-items: center; gap: 8px; }
.room-name { font-family: var(--font-mono); font-weight: 600; font-size: 13px; color: var(--color-text-primary); }
.room-n { margin-left: auto; font-family: var(--font-mono); font-size: 11px; color: var(--color-text-secondary); }
.room-desc { font-size: 11.5px; color: var(--color-text-secondary); line-height: 1.4; }
.room-members { display: flex; flex-wrap: wrap; gap: 5px; }
.room-members .chip { font-size: 11px; padding: 2px 7px; border: 1px solid var(--color-border); border-radius: var(--radius-pill, 999px); background: var(--color-base-100); }
.room-empty { font-family: var(--font-mono); font-size: 11px; color: var(--color-text-secondary); opacity: 0.6; }
"#;
