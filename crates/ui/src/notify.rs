//! Live inbound notifications for the operator.
//!
//! When the console is online and a peer sends it a message, we surface a
//! notification (pulse-leptos-ui's footer bell). Rather than subscribe to the
//! daemon's raw `NotifyChannel` push, we watch the `Message` query we already
//! hold and toast anything freshly addressed to the console session — same
//! effect, one fewer moving part. History is seeded on first load so opening
//! the dashboard doesn't dump every past message as a "new" notification.

use leptos::prelude::*;
use marshal_entities::GetAllMessages;
use pulse_leptos_ui::{NotificationLevel, NotificationWidget, Notifications};
use std::collections::HashSet;

use crate::console::use_console;
use crate::nick::use_nicknames;

/// Create the notification store, provide it, and start the inbound watch.
/// Call once in `App`, after `provide_console` + `provide_nicknames`.
pub fn provide_notifications() {
    let notifs = Notifications::new();
    provide_context(notifs.clone());

    let console = use_console();
    let nick = use_nicknames();
    let messages = myko_leptos::live_query::<GetAllMessages>(|| GetAllMessages {});

    // Message ids already accounted for — seeded with all history on the first
    // pass (so we never toast the backlog), then only genuinely-new ids toast.
    let seen: StoredValue<Option<HashSet<String>>> = StoredValue::new(None);

    Effect::new(move |_| {
        let msgs = messages.get();
        let me = console.session_id.to_string();
        let online = console.online.get();
        seen.update_value(|state| match state {
            None => {
                *state = Some(msgs.iter().map(|m| m.id.0.to_string()).collect());
            }
            Some(known) => {
                for m in &msgs {
                    let id = m.id.0.to_string();
                    if !known.insert(id) {
                        continue; // already seen
                    }
                    if !online {
                        continue; // watcher mode — nothing addressed to us
                    }
                    let to_me = m
                        .to_session_id
                        .as_ref()
                        .map(|s| s.0.as_ref() == me)
                        .unwrap_or(false);
                    if to_me && m.from_session_id.0.as_ref() != me {
                        let from = nick.of(m.from_session_id.0.as_ref());
                        notifs.push(NotificationLevel::Info, format!("{from}: {}", m.body));
                    }
                }
            }
        });
    });
}

/// The footer notification bell. Mounted once at the app root.
#[component]
pub fn NotificationBell() -> impl IntoView {
    let notifs = use_context::<Notifications>().expect("provide_notifications() at root");
    view! { <NotificationWidget notifications=notifs /> }
}
