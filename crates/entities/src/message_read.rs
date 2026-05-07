//! `MessageRead` — per-recipient read acknowledgment for any `Message`.
//!
//! Replaces the old single-bool `Message.read_at` so broadcast messages
//! (one row addressed to a room, fanned out to N members) can track
//! per-member ack without ambiguity. A message with zero `MessageRead`
//! rows is unread by everyone; a 1:1 message gets at most one row; a
//! broadcast can have up to N (one per room member who's caught up).
//!
//! Composite id is `"<message_id>::<session_id>"`, so re-acking is
//! idempotent.

use myko::myko_item;

use crate::{message::Message, session::Session};

#[myko_item]
pub struct MessageRead {
    #[belongs_to(Message)]
    pub message_id: crate::message::MessageId,

    #[belongs_to(Session)]
    pub session_id: crate::session::SessionId,

    /// Wall-clock millis when this session marked the message read.
    pub read_at: i64,
}

impl MessageRead {
    /// Compute the composite id for a (message, session) pair.
    pub fn make_id(message_id: &str, session_id: &str) -> String {
        format!("{message_id}::{session_id}")
    }
}
