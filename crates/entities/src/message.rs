use myko::myko_item;
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};

use crate::session::SessionId;

/// Maximum body size placed directly into a model-facing notification.
///
/// The durable `Message` always retains the complete body. Delivery paths use
/// a preview so a mistaken log dump or generated artifact cannot consume an
/// unbounded amount of the recipient's transcript; the full message remains
/// available through `marshal://messages`.
pub const CONTEXT_BODY_MAX_CHARS: usize = 2_000;

/// Return a UTF-8-safe model-context preview and whether it was truncated.
pub fn context_preview(body: &str, max_chars: usize) -> (String, bool) {
    let Some((byte_end, _)) = body.char_indices().nth(max_chars) else {
        return (body.to_string(), false);
    };
    (body[..byte_end].to_string(), true)
}

/// A message in the bus. Polymorphic recipient — either a peer session
/// (direct send) or a room (broadcast). Exactly one of `to_session_id`
/// and `to_room_id` is set; serde defaults to `None` on the absent
/// side so legacy direct-only payloads from before broadcast existed
/// replay clean.
///
/// Read state lives on the `MessageRead` join entity, not here, so a
/// broadcast can have per-recipient acks without ambiguity.
///
/// No denormalized display fields. Sender/recipient labels are computed
/// at read time from the live Session row (and degrade to the session_id
/// itself when the row is gone). Snapshotting nicknames at send time was
/// the source of consistency drift after rename/disconnect; with
/// session_id as the sole stored identity there is nothing to drift.
#[myko_item]
pub struct Message {
    /// Sender's session id. NOT a `belongs_to(Session)` cascade: a sent
    /// message must survive the sender exiting so the recipient can still
    /// pull it. In the pull-via-hook model the recipient reads on its
    /// next turn — which can be after the sender's SessionEnd — so
    /// cascading on the sender would silently delete unread messages in
    /// that window. The message's lifetime is governed by the *recipient*
    /// instead (the `to_session_id` / `to_room_id` cascades below).
    pub from_session_id: SessionId,

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

    /// When this direct message was addressed to a *human* — via their operator
    /// identity (the `op:`/`human:`/email tier of recipient resolution) rather
    /// than to one specific agent — this holds that operator string (e.g.
    /// `max@lucid.rocks`). The message is still routed to, and cascades on, the
    /// operator's most-active session via `to_session_id`; this marks it as
    /// human-addressed so the receiving agent surfaces it to its operator
    /// instead of treating it as ordinary peer chatter, and the UI console can
    /// toast it for that operator regardless of which agent it landed on. `None`
    /// for agent-to-agent mail. serde-default so pre-field payloads replay clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_operator: Option<String>,

    pub body: String,

    /// Wall-clock millis when the daemon accepted the message.
    pub sent_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_preview_is_bounded_and_utf8_safe() {
        assert_eq!(context_preview("short", 10), ("short".into(), false));
        assert_eq!(context_preview("aé日z", 3), ("aé日".into(), true));
    }
}
