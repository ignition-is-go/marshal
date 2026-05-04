use myko::myko_item;

use crate::session::{Session, SessionId};

#[myko_item]
pub struct Message {
    /// Sender's session id. Owned-by so the message disappears with the
    /// sender (sessions are ephemeral; we don't keep dangling messages).
    #[belongs_to(Session)]
    pub from_session_id: SessionId,

    /// Sender's nickname captured at send time, denormalized so the recipient
    /// still knows who sent it after the sender disconnects.
    pub from_nick: String,

    /// Recipient's session id. Cascade-deletes the message on recipient go.
    #[belongs_to(Session)]
    pub to_session_id: SessionId,

    pub to_nick: String,

    pub body: String,

    /// Wall-clock millis when the daemon accepted the message.
    pub sent_at: i64,

    /// Wall-clock millis when the recipient marked it read; None until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[myko_setter]
    pub read_at: Option<i64>,
}
