//! marshal-ui — the coordination console.
//!
//! A live operator dashboard over the marshal daemon: who's on the roster, what
//! they're doing, the message bus between them, and the rooms that route it.
//! Connects over WebSocket (via myko-leptos) and subscribes to the live
//! `Session`, `Message`, `Room`, `RoomMember`, and `SessionNickname` queries.
//!
//! Built on the Pulse UI stack like the other cell dashboards
//! (pulse-cluster-ui / pulse-ops-ui): `pulse-leptos-ui` for the design tokens +
//! components, and `mullion` for the dockable/splittable pane workspace. Each
//! view is a self-contained panel that pulls its own live queries, so panes can
//! be split, resized, docked, and rearranged freely.
//!
//! Read-only for now — like the TUI, the console is a watcher, not a roster
//! participant (it creates no `Session`).

use leptos::prelude::*;
use leptos_meta::{provide_meta_context, Title};
use mullion::{
    ActivityDef, ActivityIcon, ActivityId, Category, CategoryId, MullionRoot, MullionTheme,
    PaneEvent, PaneId, PaneNode, SplitDirection,
};
use pulse_leptos_ui::{BaseStyle, Status, StatusVariant};
use serde::{Deserialize, Serialize};
use styleable::Styleable;

mod console;
mod messages;
mod nick;
mod notify;
mod palette;
mod roster;
mod rooms;
mod session_detail;
mod time;

/// The marshal daemon's myko WS port (co-located with the daemon; see
/// `daemon_address`). Groups with the daemon's :6155 and the hook :6156.
const DAEMON_PORT: u16 = 6155;

/// The global focus — a lens shared across every pane. Selecting a session (a
/// roster row) or a room (a rooms card) focuses the whole workspace on it: the
/// session detail drills in, the message feed scopes to it, the console
/// pre-targets it. Session and room are mutually exclusive — focusing one clears
/// the other, so there's always a single subject.
#[derive(Clone, Copy)]
pub struct Focus {
    pub session: RwSignal<Option<String>>,
    pub room: RwSignal<Option<String>>,
}

impl Focus {
    pub fn set_session(&self, id: String) {
        self.room.set(None);
        self.session.set(Some(id));
    }
    pub fn set_room(&self, id: String) {
        self.session.set(None);
        self.room.set(Some(id));
    }
    pub fn toggle_session(&self, id: String) {
        if self.session.get_untracked().as_deref() == Some(id.as_str()) {
            self.session.set(None);
        } else {
            self.set_session(id);
        }
    }
    pub fn toggle_room(&self, id: String) {
        if self.room.get_untracked().as_deref() == Some(id.as_str()) {
            self.room.set(None);
        } else {
            self.set_room(id);
        }
    }
    pub fn clear(&self) {
        self.session.set(None);
        self.room.set(None);
    }
}

/// Read the shared focus from context.
pub fn use_focus() -> Focus {
    use_context::<Focus>().expect("Focus provided at root")
}

/// The daemon WS address to dial. marshal-01 serves both this bundle and the
/// daemon, so in the browser we connect back to the host that served the page
/// on the daemon's port — keeping the built bundle independent of which host it
/// lands on. Falls back to the dev default under `trunk serve` (localhost).
fn daemon_address() -> String {
    let host = web_sys::window()
        .and_then(|w| w.location().hostname().ok())
        .unwrap_or_default();
    if !host.is_empty() && host != "localhost" && host != "127.0.0.1" {
        format!("{host}:{DAEMON_PORT}")
    } else {
        format!("localhost:{DAEMON_PORT}")
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    // Inject the pulse-leptos-ui design tokens + reset once at root.
    let base_css = BaseStyle::default().css();

    // One shared 1 Hz clock for the whole console — drives every freshness age.
    time::provide_now();

    // Connect to the marshal daemon, then open the shared nickname subscription
    // (must follow provide_myko — it opens a live query). Cross-panel selection
    // is a single shared signal.
    let address = daemon_address();
    myko_leptos::provide_myko(&address);
    let connected = myko_leptos::use_connection_status();
    nick::provide_nicknames();
    console::provide_console();
    provide_context(Focus {
        session: RwSignal::new(None),
        room: RwSignal::new(None),
    });
    notify::provide_notifications();

    // mullion workspace theme — wired straight to the pulse-leptos-ui BaseStyle
    // tokens (injected at root) so the panes use the brand palette. The `--ml-*`
    // vars just alias the `--color-*` brand tokens.
    provide_context(MullionTheme {
        bg: "var(--color-base-100)".into(),
        surface: "var(--color-base-200)".into(),
        border: "var(--color-border)".into(),
        accent: "var(--color-base-250)".into(),
        highlight: "var(--color-base-300)".into(),
        text: "var(--color-text-primary)".into(),
        text_muted: "var(--color-text-secondary)".into(),
        drop_indicator: "color-mix(in oklch, var(--color-primary) 22%, transparent)".into(),
    });

    view! {
        <Title text="marshal"/>
        <style>{base_css}</style>
        <div class="app-shell">
            <Header connected=connected.into() addr=address.clone() />
            <main class="app-main">
                <MullionRoot
                    initial_tree=load_layout()
                    categories=categories()
                    on_event=|ev| {
                        // Persist the full tree after every mutation so drags,
                        // resizes, splits, and view-switches survive a reload.
                        if let PaneEvent::TreeChanged { tree } = ev {
                            save_layout(&tree);
                        }
                    }
                    app_icon=ActivityIcon::Svg(ICON_APP.to_string())
                />
            </main>
            <notify::NotificationBell />
            <palette::CommandK />
        </div>

        <style>{SHELL_CSS}</style>
    }
}

/// Per-pane data. Every view is self-contained (each pulls its own live queries
/// from myko context + the shared clock/nickname/selection contexts), so panes
/// carry no payload — `Slot` is just the unit the mullion tree is generic over.
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
struct Slot;

/// The four console views, registered as activities under one category. Each
/// renders inside a `pane-scroll` wrapper because the pane content area clips
/// overflow — the view scrolls within its pane. `render` is a non-capturing fn
/// (the panel component pulls its own context), so no closure state is needed.
fn categories() -> Vec<Category<Slot>> {
    vec![Category {
        id: CategoryId::new("views"),
        name: "Views".into(),
        order: 0,
        icon: ActivityIcon::Svg(ICON_APP.to_string()),
        color: "var(--color-primary)".into(),
        activities: vec![
            ActivityDef {
                id: ActivityId::new("roster"),
                name: "Roster".into(),
                icon: ActivityIcon::Svg(ICON_USERS.to_string()),
                filter: |_| true,
                render: |_p, _d| {
                    view! { <div class="pane-scroll"><roster::RosterPanel /></div> }.into_any()
                },
            },
            ActivityDef {
                id: ActivityId::new("messages"),
                name: "Messages".into(),
                icon: ActivityIcon::Svg(ICON_CHAT.to_string()),
                filter: |_| true,
                render: |_p, _d| {
                    view! { <div class="pane-scroll"><messages::MessagesPanel /></div> }.into_any()
                },
            },
            ActivityDef {
                id: ActivityId::new("rooms"),
                name: "Rooms".into(),
                icon: ActivityIcon::Svg(ICON_HASH.to_string()),
                filter: |_| true,
                render: |_p, _d| {
                    view! { <div class="pane-scroll"><rooms::RoomsPanel /></div> }.into_any()
                },
            },
            ActivityDef {
                id: ActivityId::new("session"),
                name: "Session".into(),
                icon: ActivityIcon::Svg(ICON_ID.to_string()),
                filter: |_| true,
                render: |_p, _d| {
                    view! { <div class="pane-scroll"><session_detail::SessionDetail /></div> }
                        .into_any()
                },
            },
            ActivityDef {
                id: ActivityId::new("console"),
                name: "Console".into(),
                icon: ActivityIcon::Svg(ICON_SEND.to_string()),
                filter: |_| true,
                render: |_p, _d| {
                    view! { <div class="pane-scroll"><console::ConsolePanel /></div> }.into_any()
                },
            },
        ],
    }]
}

/// localStorage key for the persisted pane layout.
const LAYOUT_KEY: &str = "marshal-ui-layout";
/// Bump when `default_layout()` changes — a persisted layout with an older
/// version is discarded so the new default reaches everyone once.
const LAYOUT_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
struct StoredLayout {
    version: u32,
    tree: PaneNode<Slot>,
}

/// Clear the persisted layout (the header's "reset layout" control).
fn clear_layout() {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = s.remove_item(LAYOUT_KEY);
    }
}

/// Restore the saved arrangement, falling back to `default_layout()` when
/// there's nothing stored, it fails to parse, its version is stale, or the URL
/// asks for the default (`?reset` / `?default`).
fn load_layout() -> PaneNode<Slot> {
    let show_default = web_sys::window()
        .and_then(|w| w.location().search().ok())
        .map(|s| s.contains("reset") || s.contains("default"))
        .unwrap_or(false);
    if show_default {
        return default_layout();
    }
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(LAYOUT_KEY).ok().flatten())
        .and_then(|json| serde_json::from_str::<StoredLayout>(&json).ok())
        .filter(|s| s.version == LAYOUT_VERSION)
        .map(|s| s.tree)
        .unwrap_or_else(default_layout)
}

/// Persist the arrangement (with the current version) to localStorage.
fn save_layout(tree: &PaneNode<Slot>) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let stored = StoredLayout {
            version: LAYOUT_VERSION,
            tree: tree.clone(),
        };
        if let Ok(json) = serde_json::to_string(&stored) {
            let _ = storage.set_item(LAYOUT_KEY, &json);
        }
    }
}

/// Default arrangement — a three-column IDE workspace:
///
/// ```text
/// ┌──────────────┬──────────────────┬─────────────┐
/// │   roster     │                  │   session   │
/// │   (hero)     │     messages     │  (detail)   │
/// ├──────────────┤     (feed)       ├─────────────┤
/// │   rooms      │                  │   console   │
/// └──────────────┴──────────────────┴─────────────┘
/// ```
///
/// Two live views get the prime real estate: the roster (hero, top-left, with
/// rooms tucked beneath) and the message feed (full height in the middle, where
/// a stream wants the vertical room). The click-to-drill session detail and the
/// operator console form a right rail — a drill-down over an action pad.
/// Drag/resize/split to taste, or switch any pane's view from its activity bar.
fn default_layout() -> PaneNode<Slot> {
    let leaf = |id: &str| PaneNode::leaf_with_activity(PaneId::new(id), ActivityId::new(id), Slot);
    // Column 1 (left): roster hero over rooms.
    let left = PaneNode::Split {
        direction: SplitDirection::Vertical,
        ratio: 0.64,
        first: Box::new(leaf("roster")),
        second: Box::new(leaf("rooms")),
    };
    // Column 3 (right rail): session detail over the operator console.
    let rail = PaneNode::Split {
        direction: SplitDirection::Vertical,
        ratio: 0.55,
        first: Box::new(leaf("session")),
        second: Box::new(leaf("console")),
    };
    // Column 2 (middle): the message feed, full height — beside the rail.
    let middle_and_rail = PaneNode::Split {
        direction: SplitDirection::Horizontal,
        ratio: 0.62,
        first: Box::new(leaf("messages")),
        second: Box::new(rail),
    };
    PaneNode::Split {
        direction: SplitDirection::Horizontal,
        ratio: 0.38,
        first: Box::new(left),
        second: Box::new(middle_and_rail),
    }
}

#[component]
fn Header(connected: Signal<bool>, addr: String) -> impl IntoView {
    let status_variant = move || {
        if connected.get() {
            StatusVariant::Success
        } else {
            StatusVariant::Error
        }
    };
    let status_label = move || if connected.get() { "connected" } else { "disconnected" };

    view! {
        <header class="app-header">
            <Status variant=Signal::derive(status_variant)>
                {move || status_label()}
            </Status>
            <span class="title">"marshal"</span>
            <span class="substrate">{addr}</span>
            <span style="flex:1"/>
            <button
                class="reset-btn"
                title="Discard your saved arrangement and return to the default layout"
                on:click=move |_| {
                    clear_layout();
                    if let Some(w) = web_sys::window() {
                        let _ = w.location().reload();
                    }
                }
            >"reset layout"</button>
        </header>
    }
}

// Activity-bar + app icons — minimal inline SVGs (stroke = currentColor).
const ICON_APP: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 8A6 6 0 0 0 6 8c0 7-3 9-3 9h18s-3-2-3-9"/><path d="M13.73 21a2 2 0 0 1-3.46 0"/></svg>"#;
const ICON_USERS: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M23 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/></svg>"#;
const ICON_CHAT: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>"#;
const ICON_HASH: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="4" y1="9" x2="20" y2="9"/><line x1="4" y1="15" x2="20" y2="15"/><line x1="10" y1="3" x2="8" y2="21"/><line x1="16" y1="3" x2="14" y2="21"/></svg>"#;
const ICON_ID: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="9" cy="10" r="2"/><path d="M15 8h2M15 12h2M7 16h10"/></svg>"#;
const ICON_SEND: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg>"#;

/// Shell chrome + the shared dense-table / stat-band / nickname grammar every
/// panel reuses. Panel-specific rules live in each panel's own `<style>`.
const SHELL_CSS: &str = r#"
.app-shell { display: flex; flex-direction: column; height: 100vh; overflow: hidden; background: var(--color-base-100); color: var(--color-text-primary); }
.app-header { display: flex; align-items: center; gap: 12px; padding: 10px 16px; border-bottom: 1px solid var(--color-border); background: var(--color-base-200); flex-shrink: 0; }
.app-header .title { font-weight: 600; letter-spacing: 0.02em; }
.app-header .substrate { color: var(--color-text-secondary); font-family: var(--font-mono); font-size: 12px; }
.app-header .reset-btn { appearance: none; border: 1px solid var(--color-border); background: var(--color-base-150); color: var(--color-text-secondary); font-family: var(--font-mono); font-size: 11px; letter-spacing: 0.02em; padding: 4px 10px; border-radius: var(--radius-sm); cursor: pointer; transition: color 0.12s, border-color 0.12s, background 0.12s; }
.app-header .reset-btn:hover { color: var(--color-text-primary); border-color: var(--color-border-hover); background: var(--color-base-100); }
.app-main { flex: 1; min-height: 0; }
.pane-scroll { height: 100%; overflow: auto; padding: 14px 16px; }

/* panel headers */
.panel-head { display: flex; align-items: baseline; gap: 10px; margin-bottom: 12px; }
.panel-head h2 { font-weight: 600; font-size: 14px; letter-spacing: 0.02em; text-transform: lowercase; }
.panel-head .sub { font-size: 11px; color: var(--color-text-secondary); font-family: var(--font-mono); }

/* ── the nickname chip — the primary identity across every panel ─────────── */
.nick { font-family: var(--font-mono); font-weight: 600; color: var(--color-primary); }
.nick.dim { color: var(--color-text-secondary); font-weight: 500; }
.sid { font-family: var(--font-mono); font-size: 10px; color: var(--color-text-secondary); opacity: 0.6; }

/* ── shared stat band ─────────────────────────────────────────────────────
   Inline group of tiles; `.crit` reddens a non-zero alarm count, `.zero` dims. */
.statband { display: inline-flex; flex-wrap: wrap; margin-bottom: 16px; border: 1px solid var(--color-border); border-radius: var(--radius-sm); overflow: hidden; background: var(--color-base-200); }
.statband .stat { min-width: 84px; display: flex; flex-direction: column; gap: 4px; padding: 10px 16px; border-right: 1px solid var(--color-border); }
.statband .stat:last-child { border-right: none; }
.statband .stat .n { font-size: 20px; font-weight: 600; line-height: 1; font-family: var(--font-mono); color: var(--color-text-primary); font-variant-numeric: tabular-nums; }
.statband .stat .l { font-size: 10px; text-transform: uppercase; letter-spacing: 0.08em; color: var(--color-text-secondary); }
.statband .stat.ok .n { color: var(--color-success); }
.statband .stat.crit .n, .statband .stat.crit .l { color: var(--color-error); }
.statband .stat.zero .n { color: var(--color-text-secondary); opacity: 0.6; }

/* ── shared dense-table grammar ───────────────────────────────────────────
   One header row of column labels, content-tight left-packed columns,
   horizontal rules only, numerics right-aligned + tabular. */
.dtable { width: 100%; border-collapse: collapse; font-family: var(--font-mono); font-size: 12px; line-height: 1.3; }
.dtable thead th { text-align: left; white-space: nowrap; font-size: 10px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.07em; color: var(--color-text-secondary); opacity: 0.75; padding: 0 14px 6px; border-bottom: 1px solid var(--color-border); position: sticky; top: 0; background: var(--color-base-100); z-index: 2; }
.dtable th.c-num, .dtable td.c-num { text-align: right; font-variant-numeric: tabular-nums; }
.dtable tbody td { padding: 6px 14px; white-space: nowrap; border-bottom: 1px solid var(--color-border); vertical-align: top; }
.dtable tbody tr:hover td { background: var(--color-base-300); }
.dtable td.dim, .dtable .dim { color: var(--color-text-secondary); }
.dtable .grp-row td { padding: 15px 14px 5px; border-bottom: none; }
.dtable tbody:first-of-type .grp-row td { padding-top: 2px; }
.dtable .grp { font-size: 11px; font-weight: 700; text-transform: uppercase; letter-spacing: 0.07em; color: var(--color-text-primary); }
.dtable .grp-n { margin-left: 8px; font-size: 10px; color: var(--color-text-secondary); }

/* ── freshness colouring on a "seen" age value ────────────────────────────── */
.seen-fresh { color: var(--color-success); }
.seen-aging { color: var(--color-warning); }
.seen-stale { color: var(--color-error); font-weight: 600; }
"#;
