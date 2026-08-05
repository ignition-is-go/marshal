//! Myko entities for the marshal coordination service.
//!
//! - `Session` — one row per connected Claude shim or TUI client.
//! - `Message` — point-to-point or broadcast text addressed to either
//!   a peer session or a room.
//! - `Room` / `RoomMember` — named groupings of sessions with M:N
//!   membership. Auto-rooms anchor on identity (`everyone`, `host:*`,
//!   `op:*`, `project:*`); ad-hoc rooms are user-created.
//! - `MessageRead` — per-recipient read acknowledgment, replaces the
//!   old single-bool `Message.read_at`.
//!
//! Persistence is configured at the daemon (see `daemon::persister`) so
//! the roster survives restart.
//!
//! Each item lives in its own module because `#[myko_item]` re-imports
//! framework traits at the parent scope; multiple items in the same
//! module would collide.

mod ack_messages;
mod broadcast_message;
mod message;
mod message_read;
mod nickname;
mod notify;
mod read_messages;
mod room;
mod room_member;
mod room_tools;
mod send_message;
mod session;
mod session_nickname;

pub use ack_messages::*;
pub use broadcast_message::*;
pub use message::*;
pub use message_read::*;
pub use nickname::*;
pub use notify::*;
pub use read_messages::*;
pub use room::*;
pub use room_member::*;
pub use room_tools::*;
pub use send_message::*;
pub use session::*;
pub use session_nickname::*;

/// Force-link this crate so the `inventory`-based item registration in the
/// generated `myko_item` code is pulled into a binary that doesn't otherwise
/// reference these types directly.
pub fn link() {}

// Register the hand-written wire structs/enums for TypeScript export. The
// `#[myko_item]`/`#[myko_command]` macros register the entities, commands, and
// their `Args` types automatically, but NOT a command's hand-written result
// type or plain helper structs — those need an explicit `register_ts_export!`
// (the same call the macros emit internally). Without this the daemon's
// `typegen` would emit `index.ts` imports for these types but never write their
// files. `register_ts_export!` is a no-op unless `ts-export` is on, so this
// costs nothing in normal builds.
myko::register_ts_export!(
    HostInfo,
    SendMessageResult,
    BroadcastMessageResult,
    DeliveredRecipient,
    FailedRecipient,
    JoinRoomResult,
    LeaveRoomResult,
    AckMessagesResult,
    ReadMessagesResult,
    MessageView,
    RoomKind,
    AutoSource,
    LivePushStatus,
    WakeStatus,
);
