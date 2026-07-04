//! `SessionNickname` — the daemon-assigned, frozen handle for a session.
//!
//! Nicknames used to be a pure function of the session id (see `nickname.rs`),
//! so changing the wordlists reshuffled every live session's handle at once.
//! This entity makes the assignment STICKY: the daemon assigns a handle the
//! first time a session appears (from the then-current wordlists,
//! collision-checked) and stores it here, keyed by session id. It is NEVER
//! reassigned — so editing/expanding the wordlists only affects sessions that
//! register afterward, and existing sessions keep their handle. Consumers READ
//! this instead of recomputing.
//!
//! The client never SETs this (it only SETs its own `Session`), so a client
//! re-SET can't clobber the assignment. Persisted + replayed like every
//! entity, so assignments survive daemon restarts. The daemon-side
//! `AssignNicknameSaga` fills it in; see `nickname.rs` for the wordlist
//! derivation the assigner starts from.

use myko::myko_item;

#[myko_item]
pub struct SessionNickname {
    /// The assigned `adjective-noun` handle, frozen for this session's life.
    /// The entity `id` is the session id it belongs to.
    pub nickname: String,
}

/// The daemon-assigned handle for `session_id`, read from the `SessionNickname`
/// store. Falls back to the computed candidate (`nickname`) for the brief
/// window before the assignment saga has run — the candidate is what the saga
/// would assign, so the two agree in the common case. Server-only.
#[cfg(not(target_arch = "wasm32"))]
pub fn nickname_for(
    ctx: &myko::command::CommandContext,
    session_id: &str,
) -> Result<String, myko::command::CommandError> {
    let assigned: Vec<std::sync::Arc<SessionNickname>> =
        ctx.exec_query(GetAllSessionNicknames {})?;
    Ok(assigned
        .iter()
        .find(|n| n.id.0.as_ref() == session_id)
        .map(|n| n.nickname.clone())
        .unwrap_or_else(|| crate::nickname(session_id)))
}
