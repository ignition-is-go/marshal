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

mod broadcast_message;
mod message;
mod message_read;
mod notify;
mod read_messages;
mod room;
mod room_member;
mod room_tools;
mod send_message;
mod session;

pub use broadcast_message::*;
pub use message::*;
pub use message_read::*;
pub use notify::*;
pub use read_messages::*;
pub use room::*;
pub use room_member::*;
pub use room_tools::*;
pub use send_message::*;
pub use session::*;

/// Force-link this crate so the `inventory`-based item registration in the
/// generated `myko_item` code is pulled into a binary that doesn't otherwise
/// reference these types directly.
pub fn link() {}
