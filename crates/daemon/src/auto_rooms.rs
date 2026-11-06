//! Daemon-side reactive auto-room maintenance.
//!
//! Every `Session` SET fires this saga, which ensures that the
//! anchor-rooms derived from the session's identity exist and that
//! the session is a member of them. The anchor kinds:
//!
//! - `everyone`              — singleton, every session always a member.
//! - `op:<operator>~<fp>`    — when `Session.operator` is populated.
//! - `project:<basename>~<fp>` — when `Session.project` is populated
//!   (the shim resolves it from `git rev-parse --show-toplevel`).
//!
//! `host:<name>` is intentionally NOT an anchor. One agent per node made
//! it a singleton in almost every case, host isn't a coordination axis
//! (it's already on each session's roster row), and it dominated the
//! auto-room count with dead weight. The GC in `cleanup.rs` reaps any
//! host rooms left over from older daemons.
//!
//! Operator and project rooms are addressed by NAME, not id: their ids
//! carry a collision-resistant fingerprint of the original identity value
//! (the sanitized display value alone is not injective — `a<b` and `ab`
//! collide, as do two values sharing a 56-character prefix). Callers that
//! reference rooms by name are resolved id-first, then by display name.
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
        // auto-room must still exist. The display name carries the sanitized
        // value, while the id and the AutoSource preserve the ORIGINAL value
        // (id via a collision-resistant fingerprint, source verbatim), so a
        // sanitized label can never make two identities share a room.
        if let Some((id, name)) = session
            .operator
            .as_deref()
            .and_then(|op| anchor_room("op:", op))
        {
            anchors.push((
                id,
                name,
                AutoSource::Operator {
                    name: session.operator.clone().unwrap_or_default(),
                },
            ));
        }
        if let Some((id, name)) = session
            .project
            .as_deref()
            .and_then(|project| anchor_room("project:", project))
        {
            anchors.push((
                id,
                name,
                AutoSource::Project {
                    basename: session.project.clone().unwrap_or_default(),
                },
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

/// The auto-room anchor for `prefix` + `value`: a bounded display name and
/// a collision-resistant room id.
///
/// The name is `prefix` + the sanitized value (forbidden characters dropped,
/// capped so the whole name fits in `ROOM_NAME_MAX_CHARS`) — the label that
/// reaches other sessions' model context. The id appends a fingerprint of
/// the ORIGINAL value, because the sanitized value alone is not injective:
/// `a<b` and `ab` sanitize to the same string, as do two values sharing the
/// 56-character prefix but differing in the tail. Without the fingerprint
/// those identities would share one room and its broadcast audience. The
/// fingerprint is deterministic across processes and restarts, so a given
/// identity always lands on the same room.
///
/// `None` when nothing of `value` survives sanitization, so a value made
/// only of forbidden characters gets no anchor, the same as an empty one.
fn anchor_room(prefix: &str, value: &str) -> Option<(RoomId, String)> {
    // The fingerprint is taken of the ORIGINAL value, before sanitization:
    // that is what makes distinct identities land on distinct ids.
    let fingerprint = fingerprint16(value);
    let cap = ROOM_NAME_MAX_CHARS.saturating_sub(prefix.chars().count());
    let value = sanitize_room_text(value, cap);
    if value.is_empty() {
        return None;
    }
    let id = format!("{prefix}{value}~{fingerprint}");
    Some((RoomId(Arc::from(id)), format!("{prefix}{value}")))
}

/// FNV-1a 64 of `value` as 16 hex characters. Deterministic across
/// processes and restarts (unlike `std`'s per-process-randomized
/// `DefaultHasher`), which the auto-room id relies on: the same identity
/// must always map to the same room.
fn fingerprint16(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint16_is_deterministic_and_value_sensitive() {
        let a = fingerprint16("a<b");
        let b = fingerprint16("ab");
        assert_ne!(a, b, "distinct original values must not share a fingerprint");
        assert_eq!(fingerprint16("a<b"), a, "deterministic across calls");
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn anchor_room_keeps_distinct_identities_apart() {
        // The reviewer's collision cases: same 56-char prefix, different
        // tail, and `a<b` vs `ab`.
        let prefix = "p".repeat(56);
        let alpha = anchor_room("project:", &format!("{prefix}-alpha")).unwrap();
        let bravo = anchor_room("project:", &format!("{prefix}-bravo")).unwrap();
        assert_eq!(alpha.1, bravo.1, "the display labels may coincide");
        assert_ne!(alpha.0, bravo.0, "the ids must not");

        let lt = anchor_room("project:", "a<b").unwrap();
        let plain = anchor_room("project:", "ab").unwrap();
        assert_ne!(lt.0, plain.0);
        assert_eq!(lt.1, "project:ab");
        assert!(lt.0.0.as_ref().starts_with("project:ab~"));

        // The same identity always lands on the same room.
        let again = anchor_room("project:", "a<b").unwrap();
        assert_eq!(lt.0, again.0);

        // A value that sanitizes to nothing gets no anchor.
        assert!(anchor_room("project:", "<\u{200b}>").is_none());
    }

    #[test]
    fn anchor_room_caps_the_display_name_but_not_the_identity() {
        let long = "é".repeat(ROOM_NAME_MAX_CHARS * 2);
        let (id, name) = anchor_room("project:", &long).unwrap();
        assert_eq!(name.chars().count(), ROOM_NAME_MAX_CHARS);
        assert!(
            id.0.as_ref().chars().count() > ROOM_NAME_MAX_CHARS,
            "the id may exceed the name cap"
        );
    }
}
