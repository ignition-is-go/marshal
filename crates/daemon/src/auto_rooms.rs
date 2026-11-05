//! Daemon-side reactive auto-room maintenance.
//!
//! Every `Session` SET fires this saga, which ensures that the
//! anchor-rooms derived from the session's identity exist and that
//! the session is a member of them. The anchor kinds:
//!
//! - `everyone`              — singleton, every session always a member.
//! - `op:<operator>`         — when `Session.operator` is populated.
//! - `project:<basename>`    — when `Session.project` is populated
//!   (the shim resolves it from `git rev-parse --show-toplevel`).
//!
//! `host:<name>` is intentionally NOT an anchor. One agent per node made
//! it a singleton in almost every case, host isn't a coordination axis
//! (it's already on each session's roster row), and it dominated the
//! auto-room count with dead weight. The GC in `cleanup.rs` reaps any
//! host rooms left over from older daemons.
//!
//! Idempotent: re-emitting a Room SET that's identical to the existing
//! row is a no-op for the registry, and the composite-id RoomMember
//! rows survive a re-SET unchanged. So the saga can fire freely on
//! every Session SET without producing churn in the event log.
//!
//! The saga runs as a `DispatchAutoRooms` server-internal command (the
//! pattern mirrors `DedupeNicknameSaga` → `DedupeNicknames`).

use std::sync::Arc;

use chrono::Utc;
use marshal_entities::{
    AutoSource, GetAllRoomMembers, GetAllRooms, ROOM_NAME_MAX_CHARS, Room, RoomId, RoomKind,
    RoomMember, RoomMemberId, Session, sanitize_room_text,
};
use myko::prelude::EventPublishing as _;
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
    prelude::myko_saga,
    saga::{SagaContext, SagaHandler},
    wire::{MEvent, MEventType},
};

/// Force-link the saga registrations from this module against
/// dead-code elimination.
pub fn link() {}

// ─── Saga ───────────────────────────────────────────────────────────────────

#[myko_saga]
pub struct AutoRoomSaga;

impl SagaHandler for AutoRoomSaga {
    type EventItem = Session;
    type Command = DispatchAutoRooms;
    const EVENT_TYPE: MEventType = MEventType::SET;

    fn handle(session: Session, _event: MEvent, _ctx: Arc<SagaContext>) -> Option<Self::Command> {
        Some(DispatchAutoRooms {
            session_id: session.id.0.as_ref().to_string(),
        })
    }
}

// ─── Command ────────────────────────────────────────────────────────────────

/// Server-internal command that ensures the four auto-rooms anchored
/// on a session's identity exist, and that the session is a member of
/// each. Pure idempotent reconciliation: missing room → SET; missing
/// membership → SET; nothing missing → no-op.
#[myko_command]
pub struct DispatchAutoRooms {
    pub session_id: String,
}

impl CommandHandler for DispatchAutoRooms {
    fn execute(self, ctx: CommandContext) -> Result<(), CommandError> {
        // Find the session row this saga was triggered by. If it's
        // gone (DEL'd between the SET and our run), bail — the saga
        // is responsible for live identity, not stale state.
        let sessions: Vec<Arc<Session>> = ctx.exec_query(marshal_entities::GetAllSessions {})?;
        let Some(session) = sessions
            .iter()
            .find(|s| s.id.0.as_ref() == self.session_id.as_str())
            .cloned()
        else {
            return Ok(());
        };

        let rooms: Vec<Arc<Room>> = ctx.exec_query(GetAllRooms {})?;
        let memberships: Vec<Arc<RoomMember>> = ctx.exec_query(GetAllRoomMembers {})?;
        let now = Utc::now().timestamp_millis();

        // Compute the desired anchor-rooms for this session.
        let mut anchors: Vec<(RoomId, String, AutoSource)> = vec![(
            RoomId(Arc::from("everyone")),
            "everyone".to_string(),
            AutoSource::Everyone,
        )];
        // The operator and project come from the client (hook query strings,
        // the cwd's repo basename) and end up in the room name, which other
        // sessions' model context renders. Sanitize rather than reject: the
        // auto-room must still exist. Id, name and source all carry the
        // sanitized value, so `id == name` keeps holding for auto-rooms.
        if let Some((name, op)) = session
            .operator
            .as_deref()
            .and_then(|op| anchor_name("op:", op))
        {
            anchors.push((
                RoomId(Arc::from(name.as_str())),
                name,
                AutoSource::Operator { name: op },
            ));
        }
        if let Some((name, basename)) = session
            .project
            .as_deref()
            .and_then(|project| anchor_name("project:", project))
        {
            anchors.push((
                RoomId(Arc::from(name.as_str())),
                name,
                AutoSource::Project { basename },
            ));
        }

        for (room_id, name, source) in anchors {
            // Ensure the Room entity exists (idempotent SET).
            let already_room = rooms.iter().any(|r| r.id == room_id);
            if !already_room {
                ctx.emit_set(&Room {
                    id: room_id.clone(),
                    name,
                    description: None,
                    kind: RoomKind::Auto { source },
                    created_at: now,
                })?;
            }

            // Ensure the membership row exists.
            let member_id = RoomMember::make_id(room_id.0.as_ref(), session.id.0.as_ref());
            let already_member = memberships
                .iter()
                .any(|m| m.id.0.as_ref() == member_id.as_str());
            if !already_member {
                ctx.emit_set(&RoomMember {
                    id: RoomMemberId(Arc::from(member_id.as_str())),
                    room_id: room_id.clone(),
                    session_id: session.id.clone(),
                    joined_at: now,
                })?;
            }
        }

        Ok(())
    }
}

/// The auto-room name for `prefix` + `value`, and the value it carries.
/// Only the value goes through the shared room text rule (forbidden
/// characters dropped), capped so the whole name fits in
/// `ROOM_NAME_MAX_CHARS`. `None` when nothing of `value` survives, so a value
/// made only of forbidden characters gets no anchor, the same as an empty one.
fn anchor_name(prefix: &str, value: &str) -> Option<(String, String)> {
    let cap = ROOM_NAME_MAX_CHARS.saturating_sub(prefix.chars().count());
    let value = sanitize_room_text(value, cap);
    (!value.is_empty()).then(|| (format!("{prefix}{value}"), value))
}
