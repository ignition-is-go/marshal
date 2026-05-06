//! Integration tests for the `SendMessage` server command.
//!
//! Spins up a real `CellServer` (no WS listener), pre-populates the
//! Session store, then invokes `SendMessage::execute` directly through a
//! synthetic `CommandContext`. The handler should:
//!   - validate the recipient session exists, returning a clear
//!     `CommandError` if not,
//!   - validate the recipient is bound to a live client (`client_id`
//!     populated), failing loudly when offline,
//!   - resolve the sender from `ctx.client_id()` and reject calls that
//!     don't map to a session,
//!   - on the happy path, emit a `Message` SET and return the
//!     `SendMessageResult` (the existing `MessageNotifySaga` handles the
//!     subsequent `NotifyChannel` push — separately tested via the
//!     end-to-end shim flow).

use std::sync::Arc;

use entities::{Message, SendMessage, Session, SessionId};
use hyphae::Gettable;
use myko::{
    command::{CommandContext, CommandHandler},
    core::item::Eventable,
    entities::client::ClientId,
    request::RequestContext,
    server::{CellServerCtx, Persister},
    wire::{MEvent, MEventType},
};
use myko_server::{BlackholePersister, CellServer};
use uuid::Uuid;

fn setup() -> CellServerCtx {
    entities::link();
    daemon::link();

    let blackhole: Arc<dyn Persister> = Arc::new(BlackholePersister);
    let server = CellServer::builder()
        .with_default_persister(blackhole)
        .build();
    let ctx = server.ctx();
    let server: &'static CellServer = Box::leak(Box::new(server));
    let _ = server;
    ctx
}

fn session(id: &str, nickname: &str, client_id: Option<&str>) -> Session {
    Session {
        id: SessionId(Arc::from(id)),
        client_id: client_id.map(|c| ClientId(Arc::from(c))),
        nickname: nickname.into(),
        pid: 0,
        cwd: "/repo".into(),
        git_branch: None,
        current_task: None,
        connected_at: 100,
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
    }
}

fn set_session(ctx: &CellServerCtx, s: &Session) {
    let event = MEvent::from_item(s, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event]).expect("apply Session SET");
}

/// Build a CommandContext as if the WS server had just dispatched a
/// command from the given client connection.
fn cmd_ctx(ctx: &CellServerCtx, caller_client_id: Option<&str>) -> CommandContext {
    let req = RequestContext::new(
        Arc::<str>::from(Uuid::new_v4().to_string().as_str()),
        caller_client_id.map(Arc::<str>::from),
        vec![Arc::<str>::from("test")],
        Uuid::new_v4(),
        chrono::Utc::now().to_rfc3339(),
    );
    CommandContext::new(
        Arc::<str>::from("SendMessage"),
        Arc::new(req),
        Arc::new(ctx.clone()),
    )
}

fn message_count(ctx: &CellServerCtx) -> usize {
    ctx.registry
        .get(Message::ENTITY_NAME_STATIC)
        .map(|store| store.entries().get().len())
        .unwrap_or(0)
}

#[test]
fn fails_loudly_when_recipient_client_binding_is_stale() {
    // Common case after the recipient bounces (self-update execve, etc.):
    // the Session row still carries an old `client_id` that is no longer
    // in the live `client_registry`. In that window the dispatch silently
    // failed under the saga-based delivery path; the new SendMessage path
    // turns it into a loud error and skips persistence.
    let ctx = setup();
    set_session(&ctx, &session("sender", "sender", Some("c-sender")));
    // `c-stale` is not registered with any live WS connection in this
    // test fixture (no real server.run()), so it stands in for a
    // post-bounce stale binding.
    set_session(&ctx, &session("recipient", "recipient", Some("c-stale")));

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: "hi".into(),
    };
    let err = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect_err("stale client binding should error");

    assert!(
        err.message.to_lowercase().contains("stale"),
        "error should call out the stale binding, got: {}",
        err.message,
    );
    assert_eq!(
        message_count(&ctx),
        0,
        "Message must not be persisted when delivery fails",
    );
}

#[test]
fn fails_loudly_when_recipient_session_id_does_not_exist() {
    let ctx = setup();
    set_session(&ctx, &session("sender", "sender", Some("c-sender")));

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("does-not-exist")),
        body: "?".into(),
    };
    let err = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect_err("missing recipient should error");

    assert!(
        err.message.contains("does-not-exist"),
        "error should name the missing session id, got: {}",
        err.message,
    );
    assert_eq!(message_count(&ctx), 0, "no Message should be persisted");
}

#[test]
fn fails_loudly_when_recipient_is_offline() {
    let ctx = setup();
    set_session(&ctx, &session("sender", "sender", Some("c-sender")));
    // Offline = client_id is None on the recipient's Session row.
    set_session(&ctx, &session("recipient", "recipient", None));

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: "?".into(),
    };
    let err = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect_err("offline recipient should error");

    assert!(
        err.message.to_lowercase().contains("offline"),
        "error should call out the offline state, got: {}",
        err.message,
    );
    assert_eq!(message_count(&ctx), 0);
}

#[test]
fn fails_loudly_when_caller_has_no_session_on_the_roster() {
    let ctx = setup();
    set_session(&ctx, &session("recipient", "recipient", Some("c-recipient")));

    // Caller's client_id has no Session bound to it on the roster.
    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: "?".into(),
    };
    let err = cmd
        .execute(cmd_ctx(&ctx, Some("c-orphan")))
        .expect_err("orphan caller should error");

    assert!(
        err.message.contains("c-orphan"),
        "error should name the unbound client id, got: {}",
        err.message,
    );
}
