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
//! Watching does not register a session. Operator messaging is opt-in, and
//! sandbox imports write separate experiment records without joining the roster.

use leptos::prelude::*;
use leptos_meta::{Title, provide_meta_context};
use mullion::{
    ActivityDef, ActivityIcon, ActivityId, ActivityNode, MullionRoot, PaneEvent, PaneId, PaneNode,
    SplitDirection,
};
use pulse_leptos_ui::{
    BaseStyle, NotificationToasts, PULSE_MULLION_CLASS, PopoverPlacement, PulseMullionStyle,
    Status, StatusVariant,
};
use pulse_multi_user_leptos::{
    ProfileMenu,
    auth::{AuthContext, AuthGate, myko_transport::provide_authenticated_myko, provide_auth},
    room::{ParticipantStack, RoomContext, provide_presence_room},
};
use serde::{Deserialize, Serialize};
use styleable::Styleable;

mod console;
mod messages;
mod nick;
mod notify;
mod palette;
mod rooms;
mod roster;
mod sandbox;
mod sandbox_run;
mod session_detail;
mod time;

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

/// Browser traffic always stays on the protected operator origin.
fn daemon_address() -> String {
    let location = web_sys::window().expect("browser window").location();
    let scheme = if location.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    format!("{scheme}://{}/myko", location.host().unwrap_or_default())
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    // Inject the pulse-leptos-ui design tokens + reset once at root.
    let base_css = BaseStyle::default().css();
    let mullion_css = PulseMullionStyle.css();

    myko_leptos::provide_myko(&daemon_address());
    let auth = provide_auth("/auth/user".into());
    view! {
        <Title text="marshal"/>
        <style>{base_css}</style>
        <style>{mullion_css}</style>
        <AuthGate auth=auth>
            <AuthenticatedApp auth=auth />
        </AuthGate>
    }
}

#[component]
fn AuthenticatedApp(auth: AuthContext) -> impl IntoView {
    // One shared 1 Hz clock for the whole console — drives every freshness age.
    time::provide_now();

    // Connect to the marshal daemon, then open the shared nickname subscription
    // (must follow provide_myko — it opens a live query). Cross-panel selection
    // is a single shared signal.
    let address = daemon_address();
    if let Err(error) = provide_authenticated_myko(&address) {
        return view! { <p role="alert">{error}</p> }.into_any();
    }
    let room = provide_presence_room(
        Signal::derive(|| Some("marshal".to_owned())),
        auth.credential,
    );
    let connected = myko_leptos::use_connection_status();
    Effect::new(move |previous: Option<bool>| {
        let current = connected.get();
        if previous == Some(true) && !current {
            auth.refresh();
        }
        current
    });
    nick::provide_nicknames();
    console::provide_console();
    provide_context(Focus {
        session: RwSignal::new(None),
        room: RwSignal::new(None),
    });
    notify::provide_notifications();

    view! {
        <div class="app-shell">
            <main class=format!("app-main {PULSE_MULLION_CLASS}")>
                <MullionRoot
                    initial_tree=load_layout()
                    items=items()
                    show_focus_indicator=true
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
            <StatusFooter connected=connected.into() auth=auth room=room />
            <NotificationToasts />
            <palette::CommandK />
        </div>

        <style>{SHELL_CSS}</style>
    }
    .into_any()
}

/// Per-pane data. Every view is self-contained (each pulls its own live queries
/// from myko context + the shared clock/nickname/selection contexts), so panes
/// carry no payload — `Slot` is just the unit the mullion tree is generic over.
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
struct Slot;

/// Console views, registered as a flat list of activities. Each
/// renders inside a `pane-scroll` wrapper because the pane content area clips
/// overflow — the view scrolls within its pane. `render` is a non-capturing fn
/// (the panel component pulls its own context), so no closure state is needed.
fn items() -> Vec<ActivityNode<Slot>> {
    vec![
        ActivityNode::activity(ActivityDef::new(
            ActivityId::new("roster"),
            "Roster",
            ActivityIcon::Svg(ICON_USERS.to_string()),
            |_| true,
            |_p, _d| view! { <div class="pane-scroll"><roster::RosterPanel /></div> }.into_any(),
        )),
        ActivityNode::activity(ActivityDef::new(
            ActivityId::new("messages"),
            "Messages",
            ActivityIcon::Svg(ICON_CHAT.to_string()),
            |_| true,
            |_p, _d| {
                view! { <div class="pane-scroll"><messages::MessagesPanel /></div> }.into_any()
            },
        )),
        ActivityNode::activity(ActivityDef::new(
            ActivityId::new("rooms"),
            "Rooms",
            ActivityIcon::Svg(ICON_HASH.to_string()),
            |_| true,
            |_p, _d| view! { <div class="pane-scroll"><rooms::RoomsPanel /></div> }.into_any(),
        )),
        ActivityNode::activity(ActivityDef::new(
            ActivityId::new("session"),
            "Session",
            ActivityIcon::Svg(ICON_ID.to_string()),
            |_| true,
            |_p, _d| {
                view! { <div class="pane-scroll"><session_detail::SessionDetail /></div> }
                    .into_any()
            },
        )),
        ActivityNode::activity(ActivityDef::new(
            ActivityId::new("sandbox"),
            "Zoo",
            ActivityIcon::Svg(ICON_FLASK.to_string()),
            |_| true,
            |_p, _d| view! { <div class="pane-scroll"><sandbox::SandboxPanel /></div> }.into_any(),
        )),
        ActivityNode::activity(ActivityDef::new(
            ActivityId::new("console"),
            "Console",
            ActivityIcon::Svg(ICON_SEND.to_string()),
            |_| true,
            |_p, _d| view! { <div class="pane-scroll"><console::ConsolePanel /></div> }.into_any(),
        )),
    ]
}

/// localStorage key for the persisted pane layout.
const LAYOUT_KEY: &str = "marshal-ui-layout";
/// Bump when `default_layout()` changes — a persisted layout with an older
/// version is discarded so the new default reaches everyone once.
const LAYOUT_VERSION: u32 = 4;

#[derive(Serialize, Deserialize)]
struct StoredLayout {
    version: u32,
    tree: PaneNode<Slot>,
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

/// Start with Roster on the left and Session on the right at the golden ratio.
fn default_layout() -> PaneNode<Slot> {
    PaneNode::Split {
        direction: SplitDirection::Horizontal,
        ratio: 0.618_033_988_749_894_8,
        first: Box::new(PaneNode::leaf_with_activity(
            PaneId::new("main"),
            ActivityId::new("roster"),
            Slot,
        )),
        second: Box::new(PaneNode::leaf_with_activity(
            PaneId::new("session"),
            ActivityId::new("session"),
            Slot,
        )),
    }
}

#[component]
fn StatusFooter(connected: Signal<bool>, auth: AuthContext, room: RoomContext) -> impl IntoView {
    let status_variant = move || {
        if connected.get() {
            StatusVariant::Success
        } else {
            StatusVariant::Error
        }
    };
    let status_label = move || {
        if connected.get() {
            "connected"
        } else {
            "disconnected"
        }
    };

    view! {
        <footer class="app-footer" data-testid="status-footer">
            <div class="footer-group" role="group" aria-label="Collaboration">
                <ProfileMenu auth=auth room=room placement=PopoverPlacement::TopStart />
                <ParticipantStack room=room />
            </div>
            <span style="flex:1"/>
            <div class="footer-group" role="group" aria-label="System status">
                <div class="footer-brand" aria-label="Marshal">
                    <span class="title">"marshal"</span>
                    <span class="version">{concat!("v", env!("CARGO_PKG_VERSION"))}</span>
                </div>
                <Status variant=Signal::derive(status_variant)>
                    {move || status_label()}
                </Status>
                <notify::NotificationBell />
            </div>
        </footer>
    }
}

// Activity-bar + app icons — minimal inline SVGs (stroke = currentColor).
const ICON_APP: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 8A6 6 0 0 0 6 8c0 7-3 9-3 9h18s-3-2-3-9"/><path d="M13.73 21a2 2 0 0 1-3.46 0"/></svg>"#;
const ICON_USERS: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M23 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/></svg>"#;
const ICON_CHAT: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>"#;
const ICON_HASH: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="4" y1="9" x2="20" y2="9"/><line x1="4" y1="15" x2="20" y2="15"/><line x1="10" y1="3" x2="8" y2="21"/><line x1="16" y1="3" x2="14" y2="21"/></svg>"#;
const ICON_ID: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="9" cy="10" r="2"/><path d="M15 8h2M15 12h2M7 16h10"/></svg>"#;
const ICON_FLASK: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 3h6M10 3v7L4 19a1 1 0 0 0 1 2h14a1 1 0 0 0 1-2l-6-9V3M7 15h10"/></svg>"#;
const ICON_SEND: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/></svg>"#;

/// Shell chrome + the shared dense-table / stat-band / nickname grammar every
/// panel reuses. Panel-specific rules live in each panel's own `<style>`.
const SHELL_CSS: &str = r#"
.app-shell { display: flex; flex-direction: column; height: 100vh; overflow: hidden; background: var(--color-base-100); color: var(--color-text-primary); }
.app-footer { position: relative; z-index: 60; flex-shrink: 0; height: var(--size-control); box-sizing: border-box; display: flex; align-items: center; padding: 0 var(--spacing-md); border-top: 1px solid var(--color-border); border-bottom: 2px solid transparent; gap: var(--spacing-md); background: var(--color-base-100); font-family: var(--font-mono); font-size: var(--font-size-2xs); line-height: 1.5; color: var(--color-text-tertiary); white-space: nowrap; }
.app-footer .footer-group { align-self: stretch; display: inline-flex; align-items: center; gap: var(--spacing-sm); min-width: 0; }
.app-footer .footer-brand { align-self: stretch; display: inline-flex; align-items: center; gap: var(--spacing-xs); padding: 0; color: var(--color-text-primary); letter-spacing: 0.08em; }
.app-footer .version { color: var(--color-text-tertiary); letter-spacing: 0; }
.app-footer .status { font-size: inherit; }
.app-main { flex: 1; min-height: 0; }
@media(max-width:600px) { .app-footer { gap:var(--spacing-sm); } }
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
