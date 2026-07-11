//! Integration tests for the `BroadcastMessage` command.
//!
//! A broadcast is delivered AMBIENTLY — one `Message` addressed to the room,
//! membership accounting only, no live push into members' turns. The one
//! exception is the `@mention` escape hatch: naming a peer `@<handle>` in the
//! body is an opt-in directed ping, delivered as a real DIRECT message
//! (persisted `to_session_id`, best-effort live push) so it's never missed —
//! even for a peer who isn't a member of the room.

use std::sync::Arc;

use hyphae::Gettable;
use marshal_entities::{
    AutoSource, BroadcastMessage, Message, Room, RoomId, RoomKind, RoomMember, RoomMemberId,
    Session, SessionId,
};
use myko::{
    command::{CommandContext, CommandHandler},
    core::item::Eventable,
    entities::client::ClientId,
    request::RequestContext,
    server::{CellServerCtx, Persister},
    utils::downcast_item,
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
    Box::leak(Box::new(server));
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
    let ev = MEvent::from_item(s, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![ev]).expect("apply Session SET");
}

fn set_room(ctx: &CellServerCtx, id: &str) {
    let room = Room {
        id: RoomId(Arc::from(id)),
        name: id.to_string(),
        description: None,
        kind: RoomKind::Auto {
            source: AutoSource::Everyone,
        },
        created_at: 0,
    };
    let ev = MEvent::from_item(&room, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![ev]).expect("apply Room SET");
}

fn set_member(ctx: &CellServerCtx, room: &str, sess: &str) {
    let m = RoomMember {
        id: RoomMemberId(Arc::from(RoomMember::make_id(room, sess).as_str())),
        room_id: RoomId(Arc::from(room)),
        session_id: SessionId(Arc::from(sess)),
        joined_at: 0,
    };
    let ev = MEvent::from_item(&m, MEventType::SET, &Uuid::new_v4().to_string());
    ctx.apply_event_batch(vec![ev]).expect("apply RoomMember SET");
}

fn cmd_ctx(ctx: &CellServerCtx, caller_client_id: Option<&str>) -> CommandContext {
    let req = RequestContext::new(
        Arc::<str>::from(Uuid::new_v4().to_string().as_str()),
        caller_client_id.map(Arc::<str>::from),
        vec![Arc::<str>::from("test")],
        Uuid::new_v4(),
        chrono::Utc::now().to_rfc3339(),
    );
    CommandContext::new(
        Arc::<str>::from("BroadcastMessage"),
        Arc::new(req),
        Arc::new(ctx.clone()),
    )
}

fn messages(ctx: &CellServerCtx) -> Vec<Message> {
    ctx.registry
        .get(Message::ENTITY_NAME_STATIC)
        .map(|s| {
            s.entries()
                .get()
                .into_iter()
                .filter_map(|(_, it)| downcast_item::<Message>(&it))
                .collect()
        })
        .unwrap_or_default()
}

/// A room with `sender` + one other member, so the broadcast has a recipient.
fn room_with_two(ctx: &CellServerCtx) {
    set_session(ctx, &session("sender", Some("c-sender")));
    set_session(ctx, &session("peer", Some("c-peer")));
    set_room(ctx, "everyone");
    set_member(ctx, "everyone", "sender");
    set_member(ctx, "everyone", "peer");
}

#[test]
fn plain_broadcast_persists_one_ambient_room_message() {
    let ctx = setup();
    room_with_two(&ctx);

    let res = BroadcastMessage {
        to_room_id: RoomId(Arc::from("everyone")),
        body: "deploy starting".into(),
        as_session: None,
    }
    .execute(cmd_ctx(&ctx, Some("c-sender")))
    .expect("broadcast ok");

    assert!(res.mentioned.is_empty(), "no @mentions → no directed pings");
    let msgs = messages(&ctx);
    assert_eq!(msgs.len(), 1, "only the ambient room message");
    assert_eq!(msgs[0].to_room_id, Some(RoomId(Arc::from("everyone"))));
    assert!(msgs[0].to_session_id.is_none(), "room message, not directed");
}

#[test]
fn mention_delivers_a_direct_ping_to_the_named_member() {
    let ctx = setup();
    set_session(&ctx, &session("sender", Some("c-sender")));
    set_session(&ctx, &session("alice", Some("c-alice")));
    set_room(&ctx, "everyone");
    set_member(&ctx, "everyone", "sender");
    set_member(&ctx, "everyone", "alice");

    let alice_nick = marshal_entities::nickname("alice");
    let res = BroadcastMessage {
        to_room_id: RoomId(Arc::from("everyone")),
        body: format!("heads up @{alice_nick}, redeploying"),
        as_session: None,
    }
    .execute(cmd_ctx(&ctx, Some("c-sender")))
    .expect("broadcast ok");

    assert_eq!(res.mentioned, vec![SessionId(Arc::from("alice"))]);
    let msgs = messages(&ctx);
    assert_eq!(msgs.len(), 2, "ambient room message + one direct ping");
    let direct = msgs
        .iter()
        .find(|m| m.to_session_id.is_some())
        .expect("a direct ping exists");
    assert_eq!(direct.to_session_id, Some(SessionId(Arc::from("alice"))));
    assert!(direct.body.starts_with("[@mention in"), "carries room context");
    assert!(direct.body.contains("redeploying"), "carries the broadcast body");
}

#[test]
fn mention_reaches_a_peer_who_is_not_a_room_member() {
    let ctx = setup();
    room_with_two(&ctx);
    // A session that is NOT a member of `everyone`.
    set_session(&ctx, &session("outsider", Some("c-out")));

    let out_nick = marshal_entities::nickname("outsider");
    let res = BroadcastMessage {
        to_room_id: RoomId(Arc::from("everyone")),
        body: format!("@{out_nick} can you take a look"),
        as_session: None,
    }
    .execute(cmd_ctx(&ctx, Some("c-sender")))
    .expect("broadcast ok");

    assert_eq!(
        res.mentioned,
        vec![SessionId(Arc::from("outsider"))],
        "a mention reaches across rooms — membership isn't required",
    );
    let direct = messages(&ctx)
        .into_iter()
        .find(|m| m.to_session_id.is_some())
        .expect("direct ping to the outsider");
    assert_eq!(direct.to_session_id, Some(SessionId(Arc::from("outsider"))));
}

#[test]
fn operator_mention_routes_to_the_humans_agent() {
    let ctx = setup();
    room_with_two(&ctx);
    // Max's agent — not a room member; addressed by operator identity.
    let mut maxs = session("max-agent", Some("c-max"));
    maxs.operator = Some("max@lucid.rocks".into());
    set_session(&ctx, &maxs);

    let res = BroadcastMessage {
        to_room_id: RoomId(Arc::from("everyone")),
        body: "@max@lucid.rocks need a call on the redeploy".into(),
        as_session: None,
    }
    .execute(cmd_ctx(&ctx, Some("c-sender")))
    .expect("broadcast ok");

    assert_eq!(
        res.mentioned,
        vec![SessionId(Arc::from("max-agent"))],
        "human-via-agent mention routes to the operator's agent",
    );
}

#[test]
fn self_mention_is_ignored() {
    let ctx = setup();
    room_with_two(&ctx);

    let sender_nick = marshal_entities::nickname("sender");
    let res = BroadcastMessage {
        to_room_id: RoomId(Arc::from("everyone")),
        body: format!("note to self @{sender_nick}"),
        as_session: None,
    }
    .execute(cmd_ctx(&ctx, Some("c-sender")))
    .expect("broadcast ok");

    assert!(res.mentioned.is_empty(), "the sender never pings itself");
    assert_eq!(messages(&ctx).len(), 1, "only the ambient room message");
}
