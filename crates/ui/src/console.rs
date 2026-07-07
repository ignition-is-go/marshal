//! Operator console — a human's way *in* to the coordination bus.
//!
//! The dashboard is a watcher by default. This module lets an operator opt in
//! to PARTICIPATE: it registers an ephemeral "operator console" `Session` on
//! the roster (so the operator has an `asSession` identity to send as) and
//! exposes the daemon's write commands — direct message, broadcast to a room,
//! and join room. That's the escape hatch for when a session is stuck, nobody's
//! at its machine, or you just need to reach a peer from a browser.
//!
//! Identity is stable per-browser (a localStorage id → the same nickname each
//! visit) with an editable operator name. Going offline (or closing the tab —
//! the daemon sweeps the stale row) removes the console from the roster. This
//! mirrors exactly how the Rust shim and the opencode plugin register: a raw
//! `SET`/`DEL` `MEvent` for the `Session`, plus a 5 s liveness heartbeat.

use leptos::prelude::*;
use marshal_entities::{
    BroadcastMessage, BroadcastMessageResult, GetAllRooms, GetAllSessions, HostInfo, JoinRoom,
    JoinRoomResult, RoomId, SendMessage, SendMessageResult, Session, SessionId,
};
use myko::client::MykoClient;
use myko::wire::event::{MEvent, MEventType};
use pulse_leptos_ui::{Button, ButtonVariant, EmptyState, Field, Select};
use std::sync::Arc;
use std::time::Duration;

use crate::nick::{self};

const SOURCE_ID: &str = "marshal-ui";
const CONSOLE_ID_KEY: &str = "marshal-ui-console-id";
const OPERATOR_KEY: &str = "marshal-ui-operator";

/// The operator console's identity + roster lifecycle. Cheap to clone (an
/// `Arc<str>` id, Copy signals, and the Clone myko client).
#[derive(Clone)]
pub struct Console {
    /// Stable per-browser session id (persisted), so the daemon assigns the
    /// same nickname on every visit.
    pub session_id: Arc<str>,
    /// Editable operator name (persisted). Anchors the `op:<name>` auto-room.
    pub operator: RwSignal<String>,
    /// Whether the console is currently ON the roster (opted in to participate).
    pub online: RwSignal<bool>,
    connected_at: RwSignal<i64>,
    client: MykoClient,
}

impl Console {
    /// The `asSession` identity for write commands.
    pub fn as_session(&self) -> SessionId {
        SessionId(self.session_id.clone())
    }

    /// The roster row this console publishes when online.
    fn session(&self) -> Session {
        let now = js_sys::Date::now() as i64;
        let operator = self.operator.get_untracked();
        Session {
            id: SessionId(self.session_id.clone()),
            client_id: None,
            pid: 0,
            cwd: "—".to_string(),
            git_branch: None,
            current_task: Some("operator console".to_string()),
            connected_at: self.connected_at.get_untracked(),
            last_activity_at: Some(now),
            last_tool: None,
            last_tool_at: None,
            operator: (!operator.is_empty()).then_some(operator),
            host: Some(HostInfo {
                name: "web-console".to_string(),
                os: "browser".to_string(),
                arch: "wasm".to_string(),
            }),
            project: Some("marshal".to_string()),
            channels_enabled: Some(false),
        }
    }

    /// SET the roster row (register / heartbeat / self-heal — idempotent).
    fn register(&self) {
        let _ = self
            .client
            .send_event(MEvent::from_item(&self.session(), MEventType::SET, SOURCE_ID));
    }

    /// DEL the roster row (leave the roster).
    fn deregister(&self) {
        let _ = self
            .client
            .send_event(MEvent::from_item(&self.session(), MEventType::DEL, SOURCE_ID));
    }

    /// Opt in to participate: stamp connect time and go online. The root effect
    /// publishes the SET once connected.
    pub fn go_online(&self) {
        self.connected_at.set(js_sys::Date::now() as i64);
        self.online.set(true);
    }

    /// Leave the roster.
    pub fn go_offline(&self) {
        self.online.set(false);
        self.deregister();
    }
}

/// Create the console identity + wire its roster lifecycle. Call once in `App`
/// AFTER `provide_myko` (it reads the myko client from context).
pub fn provide_console() {
    let client = expect_context::<MykoClient>();
    let session_id: Arc<str> = Arc::from(load_or_make_console_id().as_str());
    let operator = RwSignal::new(load_operator());
    let console = Console {
        session_id,
        operator,
        online: RwSignal::new(false),
        connected_at: RwSignal::new(js_sys::Date::now() as i64),
        client,
    };
    provide_context(console.clone());

    // Persist operator edits.
    Effect::new(move |_| {
        save_operator(&operator.get());
    });

    let connected = myko_leptos::use_connection_status();

    // Register (or re-register) whenever online + connected — this also re-runs
    // on operator change (re-SET with the new name) and on reconnect (the
    // daemon's roster is in-memory and dropped on restart).
    let c_reg = console.clone();
    Effect::new(move |_| {
        let _ = operator.get(); // track: re-SET when the name changes
        if c_reg.online.get() && connected.get() {
            c_reg.register();
        }
    });

    // Liveness heartbeat: keep the row fresh so the daemon sweeper doesn't reap
    // an online console. Untracked reads — the interval is the reactivity here.
    let c_hb = console.clone();
    set_interval(
        move || {
            if c_hb.online.get_untracked() && connected.get_untracked() {
                c_hb.register();
            }
        },
        Duration::from_secs(5),
    );
}

/// Read the console from context. Panics only if `provide_console()` wasn't
/// called at the root — a wiring bug.
pub fn use_console() -> Console {
    use_context::<Console>().expect("provide_console() must run at the app root")
}

// ── localStorage helpers ─────────────────────────────────────────────────────

fn storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

fn load_or_make_console_id() -> String {
    if let Some(existing) = storage()
        .and_then(|s| s.get_item(CONSOLE_ID_KEY).ok().flatten())
        .filter(|s| !s.is_empty())
    {
        return existing;
    }
    // Stable, collision-resistant enough for a per-browser console handle.
    let id = format!(
        "console-{:x}-{:x}",
        js_sys::Date::now() as u64,
        (js_sys::Math::random() * 4_294_967_296.0) as u64
    );
    if let Some(s) = storage() {
        let _ = s.set_item(CONSOLE_ID_KEY, &id);
    }
    id
}

fn load_operator() -> String {
    storage()
        .and_then(|s| s.get_item(OPERATOR_KEY).ok().flatten())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "operator".to_string())
}

fn save_operator(op: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(OPERATOR_KEY, op);
    }
}

// ── The Console panel ─────────────────────────────────────────────────────────

#[component]
pub fn ConsolePanel() -> impl IntoView {
    let console = use_console();
    let sessions = myko_leptos::live_query::<GetAllSessions>(|| GetAllSessions {});
    let rooms = myko_leptos::live_query::<GetAllRooms>(|| GetAllRooms {});
    let nick = nick::use_nicknames();

    let note = RwSignal::new(Option::<String>::None);
    let err = RwSignal::new(Option::<String>::None);

    // Recipient/room options (value = id, label = nickname / room name).
    let my_id = console.session_id.clone();
    let session_options = Signal::derive(move || {
        let mut opts: Vec<(String, String)> = sessions
            .get()
            .iter()
            .filter(|s| s.id.0 != my_id) // don't DM yourself
            .map(|s| (s.id.0.to_string(), nick.of(s.id.0.as_ref())))
            .collect();
        opts.sort_by(|a, b| a.1.cmp(&b.1));
        opts
    });
    let room_options = Signal::derive(move || {
        let mut opts: Vec<(String, String)> = rooms
            .get()
            .iter()
            .map(|r| (r.id.0.to_string(), format!("#{}", r.name)))
            .collect();
        opts.sort_by(|a, b| a.1.cmp(&b.1));
        opts
    });

    view! {
        <div class="panel-head">
            <h2>"console"</h2>
            <span class="sub">
                {let c = console.clone(); move || if c.online.get() { "on the roster".to_string() } else { "watching".to_string() }}
            </span>
        </div>

        <Identity console=console.clone() />

        {
            let console = console.clone();
            move || {
                if !console.online.get() {
                    return view! {
                        <div class="con-hint">
                            <EmptyState message="Go online to send messages, broadcast to rooms, and join rooms. Until then the dashboard is read-only.".to_string() />
                        </div>
                    }.into_any();
                }
                view! {
                    <div class="con-forms">
                        <Compose console=console.clone() session_options=session_options room_options=room_options note=note err=err />
                        <JoinRoomForm console=console.clone() note=note err=err />
                    </div>
                }.into_any()
            }
        }

        {move || err.get().map(|e| view! { <div class="con-err">"failed: " {e}</div> })}
        {move || note.get().map(|m| view! { <div class="con-note">{m}</div> })}

        <style>{CONSOLE_CSS}</style>
    }
}

/// Operator name + online/offline toggle.
#[component]
fn Identity(console: Console) -> impl IntoView {
    let op = console.operator;
    let c_toggle = console.clone();
    let online = console.online;
    view! {
        <div class="con-identity">
            <div class="con-field">
                <span class="con-label">"operator"</span>
                <Field
                    value=Signal::derive(move || op.get())
                    on_change=Callback::new(move |v: String| op.set(v))
                    input_type="text"
                />
            </div>
            <Button
                variant=Signal::derive(move || if online.get() { ButtonVariant::Neutral } else { ButtonVariant::Primary })
                on_click=move |_| {
                    if c_toggle.online.get() { c_toggle.go_offline() } else { c_toggle.go_online() }
                }
            >
                {move || if online.get() { "go offline".to_string() } else { "go online".to_string() }}
            </Button>
        </div>
    }
}

/// Direct-message a session or broadcast to a room.
#[component]
fn Compose(
    console: Console,
    session_options: Signal<Vec<(String, String)>>,
    room_options: Signal<Vec<(String, String)>>,
    note: RwSignal<Option<String>>,
    err: RwSignal<Option<String>>,
) -> impl IntoView {
    let sink = myko_leptos::use_command_sink();

    // DM state.
    let dm_to = RwSignal::new(String::new());
    let dm_body = RwSignal::new(String::new());
    // Broadcast state.
    let bc_room = RwSignal::new(String::new());
    let bc_body = RwSignal::new(String::new());

    let as_session = console.as_session();

    let sink_dm = sink.clone();
    let as_dm = as_session.clone();
    let send_dm = move || {
        let (to, body) = (dm_to.get_untracked(), dm_body.get_untracked());
        if to.is_empty() || body.trim().is_empty() {
            return;
        }
        note.set(None);
        err.set(None);
        sink_dm.dispatch::<SendMessage, SendMessageResult>(
            SendMessage {
                to_session_id: SessionId(Arc::from(to.as_str())),
                body,
                as_session: Some(as_dm.clone()),
            },
            move |res| match res {
                Ok(r) => {
                    note.set(Some(format!("sent ✓ (delivered_live={})", r.delivered_live)));
                    dm_body.set(String::new());
                }
                Err(e) => err.set(Some(e)),
            },
        );
    };

    let sink_bc = sink.clone();
    let as_bc = as_session.clone();
    let send_bc = move || {
        let (room, body) = (bc_room.get_untracked(), bc_body.get_untracked());
        if room.is_empty() || body.trim().is_empty() {
            return;
        }
        note.set(None);
        err.set(None);
        sink_bc.dispatch::<BroadcastMessage, BroadcastMessageResult>(
            BroadcastMessage {
                to_room_id: RoomId(Arc::from(room.as_str())),
                body,
                as_session: Some(as_bc.clone()),
            },
            move |res| match res {
                Ok(r) => {
                    note.set(Some(format!("broadcast ✓ ({} delivered / {} total)", r.delivered.len(), r.total)));
                    bc_body.set(String::new());
                }
                Err(e) => err.set(Some(e)),
            },
        );
    };

    view! {
        <div class="con-card">
            <div class="con-card-h">"direct message"</div>
            <Select
                value=Signal::derive(move || dm_to.get())
                on_change=move |v: String| dm_to.set(v)
                options=session_options
                placeholder="pick a session"
            />
            <textarea
                class="con-body"
                placeholder="message…"
                prop:value=move || dm_body.get()
                on:input=move |e| dm_body.set(event_target_value(&e))
            />
            <Button
                variant=ButtonVariant::Primary
                disabled=Signal::derive(move || dm_to.get().is_empty() || dm_body.get().trim().is_empty())
                on_click=move |_| send_dm()
            >"send"</Button>
        </div>

        <div class="con-card">
            <div class="con-card-h">"broadcast"</div>
            <Select
                value=Signal::derive(move || bc_room.get())
                on_change=move |v: String| bc_room.set(v)
                options=room_options
                placeholder="pick a room"
            />
            <textarea
                class="con-body"
                placeholder="message to the room…"
                prop:value=move || bc_body.get()
                on:input=move |e| bc_body.set(event_target_value(&e))
            />
            <Button
                variant=ButtonVariant::Accent
                disabled=Signal::derive(move || bc_room.get().is_empty() || bc_body.get().trim().is_empty())
                on_click=move |_| send_bc()
            >"broadcast"</Button>
        </div>
    }
}

/// Create or join an ad-hoc room.
#[component]
fn JoinRoomForm(
    console: Console,
    note: RwSignal<Option<String>>,
    err: RwSignal<Option<String>>,
) -> impl IntoView {
    let sink = myko_leptos::use_command_sink();
    let name = RwSignal::new(String::new());
    let as_session = console.as_session();

    let do_join = move || {
        let n = name.get_untracked();
        if n.trim().is_empty() {
            return;
        }
        note.set(None);
        err.set(None);
        sink.dispatch::<JoinRoom, JoinRoomResult>(
            JoinRoom {
                name: n,
                description: None,
                as_session: Some(as_session.clone()),
            },
            move |res| match res {
                Ok(r) => {
                    note.set(Some(format!("{} #{}", if r.created { "created" } else { "joined" }, r.name)));
                    name.set(String::new());
                }
                Err(e) => err.set(Some(e)),
            },
        );
    };

    view! {
        <div class="con-card">
            <div class="con-card-h">"join room"</div>
            <div class="con-join">
                <Field
                    value=Signal::derive(move || name.get())
                    on_change=Callback::new(move |v: String| name.set(v))
                    input_type="text"
                    placeholder="room name"
                />
                <Button
                    variant=ButtonVariant::Secondary
                    disabled=Signal::derive(move || name.get().trim().is_empty())
                    on_click=move |_| do_join()
                >"join"</Button>
            </div>
        </div>
    }
}

/// A compact reply box the Session detail pane embeds to DM the selected
/// session. Requires the console to be online; prompts otherwise.
#[component]
pub fn ReplyBox(target: String) -> impl IntoView {
    let console = use_console();
    let sink = myko_leptos::use_command_sink();
    let body = RwSignal::new(String::new());
    let note = RwSignal::new(Option::<String>::None);
    let err = RwSignal::new(Option::<String>::None);
    let as_session = console.as_session();
    let online = console.online;

    // A Callback (Copy) so the reactive online/offline view below can reuse it
    // on every re-render without moving a plain closure.
    let send = Callback::new(move |()| {
        let b = body.get_untracked();
        if b.trim().is_empty() {
            return;
        }
        note.set(None);
        err.set(None);
        sink.dispatch::<SendMessage, SendMessageResult>(
            SendMessage {
                to_session_id: SessionId(Arc::from(target.as_str())),
                body: b,
                as_session: Some(as_session.clone()),
            },
            move |res| match res {
                Ok(_) => {
                    note.set(Some("sent ✓".to_string()));
                    body.set(String::new());
                }
                Err(e) => err.set(Some(e)),
            },
        );
    });

    let c_online = console.clone();
    view! {
        <div class="reply">
            {move || if !online.get() {
                let c = c_online.clone();
                view! {
                    <div class="reply-off">
                        "read-only — "
                        <button class="reply-link" on:click=move |_| c.go_online()>"go online"</button>
                        " to reply"
                    </div>
                }.into_any()
            } else {
                view! {
                    <div class="reply-live">
                        <textarea
                            class="con-body"
                            placeholder="reply…"
                            prop:value=move || body.get()
                            on:input=move |e| body.set(event_target_value(&e))
                        />
                        <Button
                            variant=ButtonVariant::Primary
                            disabled=Signal::derive(move || body.get().trim().is_empty())
                            on_click=move |_| send.run(())
                        >"send"</Button>
                        {move || err.get().map(|e| view! { <span class="con-err">{e}</span> })}
                        {move || note.get().map(|m| view! { <span class="con-note">{m}</span> })}
                    </div>
                }.into_any()
            }}
        </div>
    }
}

const CONSOLE_CSS: &str = r#"
.con-identity { display: flex; align-items: flex-end; gap: 12px; margin-bottom: 16px; }
.con-field { display: flex; flex-direction: column; gap: 5px; }
.con-label { font-size: 10px; text-transform: uppercase; letter-spacing: 0.07em; color: var(--color-text-secondary); font-family: var(--font-mono); }
.con-hint { margin-top: 8px; }
.con-forms { display: flex; flex-direction: column; gap: 14px; }
.con-card { border: 1px solid var(--color-border); border-radius: var(--radius-sm); background: var(--color-base-200); padding: 12px; display: flex; flex-direction: column; gap: 8px; }
.con-card-h { font-size: 10px; text-transform: uppercase; letter-spacing: 0.08em; color: var(--color-text-secondary); font-weight: 700; }
.con-body { width: 100%; min-height: 54px; resize: vertical; background: var(--color-base-100); border: 1px solid var(--color-border); border-radius: var(--radius-sm); color: var(--color-text-primary); font-family: var(--font-mono); font-size: 12px; padding: 8px 10px; line-height: 1.4; }
.con-body:focus { outline: none; border-color: var(--color-border-strong); }
.con-join { display: flex; align-items: flex-end; gap: 8px; }
.con-err { color: var(--color-error); font-family: var(--font-mono); font-size: 11.5px; margin-top: 8px; }
.con-note { color: var(--color-info); font-family: var(--font-mono); font-size: 11.5px; margin-top: 8px; }
.reply { margin-top: 12px; border-top: 1px solid var(--color-border); padding-top: 12px; }
.reply-off { font-family: var(--font-mono); font-size: 11.5px; color: var(--color-text-secondary); }
.reply-link { appearance: none; background: none; border: none; color: var(--color-primary); font-family: var(--font-mono); font-size: 11.5px; cursor: pointer; padding: 0; text-decoration: underline; }
.reply-live { display: flex; flex-direction: column; gap: 8px; }
"#;
