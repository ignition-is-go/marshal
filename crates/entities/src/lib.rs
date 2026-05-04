//! Myko entities for the claude-coord coordination service.
//!
//! - `Session` — one row per connected Claude shim or TUI client.
//! - `Message` — point-to-point text passed between sessions.
//!
//! No persistence is configured: a server restart drops everything.
//!
//! Each item lives in its own module because `#[myko_item]` re-imports
//! framework traits at the parent scope; multiple items in the same module
//! would collide.

mod session;
mod message;

pub use message::*;
pub use session::*;

/// Force-link this crate so the `inventory`-based item registration in the
/// generated `myko_item` code is pulled into a binary that doesn't otherwise
/// reference these types directly.
pub fn link() {}
