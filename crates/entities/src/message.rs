use myko::myko_item;

use crate::session::SessionId;

/// A message in the bus. Polymorphic recipient — either a peer session
/// (direct send) or a room (broadcast). Exactly one of `to_session_id`
/// and `to_room_id` is set; serde defaults to `None` on the absent
/// side so legacy direct-only payloads from before broadcast existed
/// replay clean.
///
/// Read state lives on the `MessageRead` join entity, not here, so a
/// broadcast can have per-recipient acks without ambiguity.
#[myko_item]
pub struct Message {
    /// Sender's session id. NOT a `belongs_to` cascade: a sent message
    /// must survive the sender exiting so the recipient can still pull it.
    /// In the pull-via-hook model the recipient reads on its next turn,
    /// which can be after the sender's SessionEnd — cascading on the
    /// sender would silently delete unread messages in that window. The
    /// message's lifetime is governed by the *recipient* instead (the
    /// `to_session_id` / `to_room_id` cascades below); `from_nick` is
    /// denormalized so we still know who sent it after the sender is gone.
    pub from_session_id: SessionId,

    /// Sender's nickname captured at send time, denormalized so the
    /// recipient still knows who sent it after the sender disconnects.
    pub from_nick: String,

    /// Direct recipient — set for 1:1 sends, `None` for broadcasts.
    /// Cascade-DELs the message when the recipient session is DEL'd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[belongs_to(Session, optional)]
    pub to_session_id: Option<SessionId>,

    /// Broadcast recipient — set for room sends, `None` for direct
    /// sends. Cascade-DELs when the room is DEL'd. (Auto-rooms like
    /// `everyone` and `host:*` are never DEL'd, so broadcasts to them
    /// effectively persist forever.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[belongs_to(Room, optional)]
    pub to_room_id: Option<crate::room::RoomId>,

    /// Display name of the recipient at send time — peer's nickname for
    /// direct sends, room's name for broadcasts. Denormalized so the
    /// transcript still reads naturally after a rename or DEL.
    pub to_nick: String,

    pub body: String,

    /// Wall-clock millis when the daemon accepted the message.
    pub sent_at: i64,
}
