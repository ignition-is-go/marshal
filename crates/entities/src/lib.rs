//! Myko entities for the marshal coordination service.
//!
//! - `Session` — one row per connected Claude shim or TUI client.
//! - `Message` — point-to-point text passed between sessions.
//!
//! Persistence is configured at the daemon (see `daemon::persister`) so the
//! roster survives restart.
//!
//! Each item lives in its own module because `#[myko_item]` re-imports
//! framework traits at the parent scope; multiple items in the same module
//! would collide.

mod message;
mod notify;
mod send_message;
mod session;

pub use message::*;
pub use notify::*;
pub use send_message::*;
pub use session::*;

/// Force-link this crate so the `inventory`-based item registration in the
/// generated `myko_item` code is pulled into a binary that doesn't otherwise
/// reference these types directly.
pub fn link() {}
