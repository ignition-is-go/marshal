//! `RoomMember` — M:N join between `Session` and `Room`.
//!
//! A row's `id` is the deterministic composite `"<room_id>::<session_id>"`,
//! so a session joining the same room twice is idempotent (the second SET
//! overwrites the first row instead of creating a parallel one). The
//! `belongs_to` cascades on both sides keep membership consistent: a
//! session DEL also DELs every membership row referencing it; a room DEL
//! does the same.

use myko::myko_item;
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};

use crate::{room::Room, session::Session};

#[myko_item]
pub struct RoomMember {
    #[belongs_to(Room)]
    pub room_id: crate::room::RoomId,

    #[belongs_to(Session)]
    pub session_id: crate::session::SessionId,

    /// Wall-clock millis when this session first joined this room.
    /// Used by `read_messages` visibility rules: members only see
    /// messages sent at or after `joined_at` so a re-join doesn't
    /// surface back-history they were never part of.
    pub joined_at: i64,
}

impl RoomMember {
    /// Compute the composite id for a (room, session) pair so callers
    /// can construct membership rows or look them up without
    /// reaching into the format directly.
    pub fn make_id(room_id: &str, session_id: &str) -> String {
        format!("{room_id}::{session_id}")
    }
}
