//! MCP tool dispatch — translates each tool call into one or more MykoClient
//! operations.
//!
//! Implementation status:
//! - whoami: returns local pid/cwd/nickname (no server roundtrip yet)
//! - set_role: returns curated instructions text; server-side mutation TBD
//! - roster: TODO — requires `client.watch_query::<GetAllSessions>` + snapshot
//! - set_status, send_message, inbox, recent_messages: TODO
//!
//! These are intentionally stub-heavy in this commit; the wiring through the
//! NotifyChannel push path is the priority.

use crate::mcp::{
    INVALID_PARAMS, METHOD_NOT_FOUND, Notifier, ToolError, ToolFuture, ToolHandler, ToolOutcome,
};
use crate::role_instructions;
use myko::client::MykoClient;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct ToolHost {
    pub client: Arc<MykoClient>,
    pub pid: u32,
    pub cwd: String,
    pub nickname: String,
}

pub struct CoordHandler {
    pub host: Arc<ToolHost>,
}

impl ToolHandler for CoordHandler {
    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        _notifier: &'a Notifier,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            match name {
                "whoami" => Ok(ToolOutcome::Json(json!({
                    "nickname": self.host.nickname,
                    "pid": self.host.pid,
                    "cwd": self.host.cwd,
                }))),

                "set_role" => {
                    let role = args
                        .get("role")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::invalid_params("set_role: missing `role`"))?
                        .to_string();
                    let canonical = role_instructions::canonicalize(&role);
                    let instructions = role_instructions::instructions(&canonical);
                    // TODO: push the role onto our Session item so peers can see
                    // it on the roster — needs the auto-generated SetSessionRole
                    // command from #[myko_setter].
                    Ok(ToolOutcome::Json(json!({
                        "role": canonical,
                        "instructions": instructions,
                    })))
                }

                "roster" => {
                    // TODO: client.watch_query::<GetAllSessions>(...)
                    Ok(ToolOutcome::Json(json!({
                        "sessions": [],
                        "note": "roster query not yet wired"
                    })))
                }

                other => Err(ToolError {
                    code: METHOD_NOT_FOUND,
                    message: format!("unknown tool: {other}"),
                    data: None,
                }),
            }
        })
    }
}

#[allow(dead_code)]
fn _invalid_params_helper() -> ToolError {
    ToolError {
        code: INVALID_PARAMS,
        message: String::new(),
        data: None,
    }
}
