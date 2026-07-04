//! Daemon-side sticky nickname assignment.
//!
//! Every `Session` SET fires this saga. The first time a session is seen it
//! assigns a frozen handle — the candidate from the current wordlists
//! (`marshal_entities::nickname`), re-hashed on the rare collision with a
//! live-or-past assignment — and stores it as a `SessionNickname`. Once
//! assigned it is NEVER touched again, so editing/expanding the wordlists only
//! affects sessions that register afterward; existing sessions keep their
//! handle. That's the grandfather: `session_nickname.rs`.
//!
//! Idempotent + cheap: on a session that already has a handle the command is
//! a single query + early return, so it's safe to fire on every Session SET
//! (the client re-SETs its Session constantly, but never touches the separate
//! `SessionNickname`, so it can't clobber the assignment).

use std::{collections::HashSet, sync::Arc};

use marshal_entities::{
    nickname, GetAllSessionNicknames, GetAllSessions, Session, SessionNickname, SessionNicknameId,
};
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
    prelude::myko_saga,
    saga::{SagaContext, SagaHandler},
    wire::{MEvent, MEventType},
};

/// Force-link the saga/command registrations against dead-code elimination.
pub fn link() {}

// ─── Saga ───────────────────────────────────────────────────────────────────

#[myko_saga]
pub struct AssignNicknameSaga;

impl SagaHandler for AssignNicknameSaga {
    type EventItem = Session;
    type Command = AssignNickname;
    const EVENT_TYPE: MEventType = MEventType::SET;

    fn handle(session: Session, _event: MEvent, _ctx: Arc<SagaContext>) -> Option<Self::Command> {
        Some(AssignNickname {
            session_id: session.id.0.as_ref().to_string(),
        })
    }
}

// ─── Command ────────────────────────────────────────────────────────────────

/// Server-internal: assign a frozen handle to `session_id` if it has none yet.
#[myko_command]
pub struct AssignNickname {
    pub session_id: String,
}

impl CommandHandler for AssignNickname {
    fn execute(self, ctx: CommandContext) -> Result<(), CommandError> {
        // The session may have DEL'd between the SET and our run — that's fine,
        // a stray assignment is harmless, but skip it to avoid reserving a name
        // for a gone session.
        let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;
        if !sessions
            .iter()
            .any(|s| s.id.0.as_ref() == self.session_id.as_str())
        {
            return Ok(());
        }

        let assigned: Vec<Arc<SessionNickname>> = ctx.exec_query(GetAllSessionNicknames {})?;

        // Already has a handle → FROZEN. This early return is the whole
        // grandfather: once a session is named, no wordlist change moves it.
        if assigned
            .iter()
            .any(|n| n.id.0.as_ref() == self.session_id.as_str())
        {
            return Ok(());
        }

        // Assign: the current-wordlist candidate, re-hashed with a salt on the
        // rare collision so two sessions never share a handle. The un-salted
        // candidate equals the old pure-function output, so at rollout every
        // live session is assigned exactly the handle it already had.
        let taken: HashSet<&str> = assigned.iter().map(|n| n.nickname.as_str()).collect();
        let mut handle = nickname(&self.session_id);
        let mut salt = 0u32;
        while taken.contains(handle.as_str()) {
            salt += 1;
            handle = nickname(&format!("{}#{salt}", self.session_id));
        }

        ctx.emit_set(&SessionNickname {
            id: SessionNicknameId(Arc::from(self.session_id.as_str())),
            nickname: handle,
        })?;
        Ok(())
    }
}
