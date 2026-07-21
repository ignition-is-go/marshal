//! Integration tests for the `SendMessage` server command (pull model).
//!
//! Spins up a real `CellServer` (no WS listener), pre-populates the
//! Session store, then invokes `SendMessage::execute` directly through a
//! synthetic `CommandContext`. Under the pull model the handler should:
//!   - resolve the sender from the WS `client_id` OR an explicit
//!     `as_session` (self-identify, for connectionless HTTP-MCP callers),
//!   - reject an unknown recipient and an unresolvable sender with a clear
//!     `CommandError` (these are the only hard failures),
//!   - **persist the `Message` regardless of whether the recipient is
//!     online** — delivery is decoupled from acceptance; an offline
//!     recipient pulls it on its next hook turn,
//!   - report `delivered_live` = whether a best-effort live push to a
//!     connected recipient landed (always `false` in these fixtures, which
//!     have no live `client_registry`).

use std::sync::Arc;

use hyphae::Gettable;
use marshal_entities::{Message, SendMessage, Session, SessionId};
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
    marshal_entities::link();
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

fn session(id: &str, client_id: Option<&str>) -> Session {
    Session {
        id: SessionId(Arc::from(id)),
        client_id: client_id.map(|c| ClientId(Arc::from(c))),
        pid: 0,
        cwd: "/repo".into(),
        git_branch: None,
        current_task: None,
        connected_at: 100,
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
        operator: None,
        host: None,
        project: None,
        channels_enabled: None,
    }
}

fn set_session(ctx: &CellServerCtx, s: &Session) {
    let event = MEvent::from_item(s, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![event])
        .expect("apply Session SET");
}

/// Build a CommandContext as if the WS server had just dispatched a
/// command from the given client connection (`None` = no connection
/// identity, the HTTP-MCP / hook path).
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

fn only_message(ctx: &CellServerCtx) -> Message {
    let store = ctx
        .registry
        .get(Message::ENTITY_NAME_STATIC)
        .expect("Message store exists");
    let entries = store.entries().get();
    assert_eq!(entries.len(), 1, "expected exactly one Message");
    myko::utils::downcast_item::<Message>(&entries.into_iter().next().unwrap().1)
        .expect("entry is a Message")
}

#[test]
fn offline_recipient_succeeds_and_persists_for_pull() {
    // Pull model: an offline recipient (no live client) is NOT an error.
    // The message persists so the recipient pulls it on its next hook turn.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient", None));

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: "hi".into(),
        as_session: None,
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("offline recipient is success under the pull model");

    assert!(
        !result.delivered_live,
        "no live client → not delivered live"
    );
    assert_eq!(message_count(&ctx), 1, "message must persist for pull");
    assert_eq!(only_message(&ctx).body, "hi");
}

#[test]
fn stale_binding_succeeds_and_persists() {
    // A recipient whose `client_id` points at a connection that is no
    // longer live (post-bounce). The best-effort push finds no live client
    // and reports `delivered_live = false`, but the message still persists.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient", Some("c-stale")));

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: "hi".into(),
        as_session: None,
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("stale binding is not a hard failure under the pull model");

    assert!(!result.delivered_live);
    assert_eq!(message_count(&ctx), 1);
}

#[test]
fn self_identified_sender_via_as_session_succeeds() {
    // The HTTP-MCP path: no connection `client_id`; the caller names itself
    // via `as_session`. Sender resolves from that, message persists, and
    // it's attributed to the self-identified session.
    let ctx = setup();
    set_session(&ctx, &session("sender", None));
    set_session(&ctx, &session("recipient", None));

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: "from http".into(),
        as_session: Some(SessionId(Arc::from("sender"))),
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, None))
        .expect("self-identified send succeeds");

    assert!(!result.delivered_live);
    let msg = only_message(&ctx);
    assert_eq!(msg.from_session_id, SessionId(Arc::from("sender")));
    assert_eq!(msg.body, "from http");
}

#[test]
fn unknown_recipient_errors_and_does_not_persist() {
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("does-not-exist")),
        body: "?".into(),
        as_session: None,
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
fn nickname_resolves_server_side() {
    // Resolution moved into the command (was shim-only) so EVERY harness —
    // opencode plugin, raw HTTP-MCP, shim — can address by nickname. No
    // SessionNickname is assigned here, so `nickname_for` falls back to the
    // deterministic handle; address by that and it must route to the session.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("recipient-xyz", None));

    let nick = marshal_entities::nickname("recipient-xyz");
    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from(nick.as_str())),
        body: "hi".into(),
        as_session: None,
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("a unique nickname resolves");

    assert_eq!(
        result.to_session_id,
        SessionId(Arc::from("recipient-xyz")),
        "nickname `{nick}` should resolve to its session id",
    );
    assert_eq!(message_count(&ctx), 1);
    // Agent-addressed (id/nickname/prefix) mail must NOT be marked
    // human-addressed — otherwise the recipient would be told to relay ordinary
    // peer chatter to a human.
    assert_eq!(
        only_message(&ctx).to_operator,
        None,
        "nickname-addressed mail is agent-to-agent, not human-addressed",
    );
}

#[test]
fn operator_token_routes_to_the_humans_most_recently_active_agent() {
    // Human-via-agent routing: address the PERSON by their operator identity
    // and the daemon picks which of their agents currently has the floor —
    // the most-recently-active one.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));

    let mut idle = session("max-idle", Some("c-idle"));
    idle.operator = Some("max@lucid.rocks".into());
    idle.last_activity_at = Some(1_000);
    let mut active = session("max-active", Some("c-active"));
    active.operator = Some("max@lucid.rocks".into());
    active.last_activity_at = Some(9_000);
    set_session(&ctx, &idle);
    set_session(&ctx, &active);

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("max@lucid.rocks")),
        body: "your call on the redeploy".into(),
        as_session: None,
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("operator identity resolves to the human's agent");

    assert_eq!(
        result.to_session_id,
        SessionId(Arc::from("max-active")),
        "should route to the most-recently-active of the operator's sessions",
    );
    let msg = only_message(&ctx);
    assert_eq!(msg.to_session_id, Some(SessionId(Arc::from("max-active"))));
    // ...and it's marked human-addressed so the receiving agent surfaces it to
    // its operator rather than treating it as ordinary peer chatter.
    assert_eq!(
        msg.to_operator.as_deref(),
        Some("max@lucid.rocks"),
        "operator-tier routing must stamp `to_operator` (human-addressed)",
    );
}

#[test]
fn operator_routing_prefers_a_live_agent_and_accepts_the_op_prefix() {
    // A disconnected session active more recently vs a live one active less
    // recently — the LIVE agent wins (that's where a push can land now). Also
    // exercises the explicit `op:` disambiguation prefix.
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));

    let mut disconnected = session("max-disc", None); // client_id None ⇒ not live
    disconnected.operator = Some("max@lucid.rocks".into());
    disconnected.last_activity_at = Some(9_000);
    let mut live = session("max-live", Some("c-live"));
    live.operator = Some("max@lucid.rocks".into());
    live.last_activity_at = Some(5_000);
    set_session(&ctx, &disconnected);
    set_session(&ctx, &live);

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("op:max@lucid.rocks")),
        body: "ping".into(),
        as_session: None,
    };
    let result = cmd
        .execute(cmd_ctx(&ctx, Some("c-sender")))
        .expect("op:-prefixed operator identity resolves");

    assert_eq!(
        result.to_session_id,
        SessionId(Arc::from("max-live")),
        "a live agent outranks a more-recently-active disconnected one",
    );
}

#[test]
fn caller_without_session_errors() {
    // Caller's client_id maps to no session and no `as_session` was given.
    let ctx = setup();
    set_session(&ctx, &session("recipient", Some("c-recipient")));

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: "?".into(),
        as_session: None,
    };
    let err = cmd
        .execute(cmd_ctx(&ctx, Some("c-orphan")))
        .expect_err("orphan caller should error");

    assert!(
        err.message.contains("c-orphan"),
        "error should name the unbound client id, got: {}",
        err.message,
    );
    assert_eq!(message_count(&ctx), 0);
}

#[test]
fn unidentified_caller_errors() {
    // No client_id AND no as_session → cannot resolve a sender at all.
    let ctx = setup();
    set_session(&ctx, &session("recipient", None));

    let cmd = SendMessage {
        to_session_id: SessionId(Arc::from("recipient")),
        body: "?".into(),
        as_session: None,
    };
    let err = cmd
        .execute(cmd_ctx(&ctx, None))
        .expect_err("no identity should error");

    assert!(
        err.message.to_lowercase().contains("assession")
            || err.message.to_lowercase().contains("connected client"),
        "error should explain the missing identity, got: {}",
        err.message,
    );
    assert_eq!(message_count(&ctx), 0);
}
