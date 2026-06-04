//! Caller-session resolution that supports both transports.
//!
//! WS-backed agents (the shim) arrive with `CommandContext.client_id`
//! set and no `mcp_session_id`. HTTP-MCP agents arrive with
//! `mcp_session_id` set and no `client_id`. Internal callers (sagas,
//! startup tasks) have neither. The lookup logic is the same in every
//! command that needs to identify its caller; extracting it here keeps
//! the per-command code small and the two-path support consistent.

use std::sync::Arc;

use myko::command::{CommandContext, CommandError};

use crate::session::Session;

/// Resolve the caller's `Session` from a `CommandContext`. Returns a
/// reference into the supplied `sessions` slice; the caller passes that
/// slice in (rather than this fn re-running `GetAllSessions`) so the
/// same roster snapshot covers both the caller lookup and any
/// follow-on recipient lookup the command needs to do.
pub fn caller_session<'a>(
    ctx: &CommandContext,
    sessions: &'a [Arc<Session>],
    command_name: &str,
) -> Result<&'a Arc<Session>, CommandError> {
    if let Some(caller_client_id) = ctx.client_id() {
        return sessions
            .iter()
            .find(|s| s.client_id.as_ref() == Some(&caller_client_id))
            .ok_or_else(|| {
                command_err(
                    ctx,
                    format!(
                        "caller (client {}) has no session on the roster — re-SET your Session and retry",
                        caller_client_id.0.as_ref(),
                    ),
                )
            });
    }

    if let Some(caller_sid) = ctx.req.mcp_session_id.as_ref() {
        return sessions
            .iter()
            .find(|s| s.id.0.as_ref() == caller_sid.as_ref())
            .ok_or_else(|| {
                command_err(
                    ctx,
                    format!(
                        "caller (mcp-session {}) has no session on the roster — \
                         reconnect to /myko/mcp and retry",
                        caller_sid.as_ref(),
                    ),
                )
            });
    }

    Err(command_err(
        ctx,
        format!(
            "{command_name} must be called from a connected client or HTTP-MCP session"
        ),
    ))
}

fn command_err(ctx: &CommandContext, message: String) -> CommandError {
    CommandError {
        tx: ctx.tx().to_string(),
        command_id: ctx.command_id.to_string(),
        message,
    }
}
