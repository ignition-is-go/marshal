//! Server→client push command. The daemon dispatches `NotifyChannel` at a
//! specific connected client when something interesting happens for that
//! session (currently: a peer sent a message). The shim's `MykoClient`
//! handles it via `on_command::<NotifyChannel, _>` and translates it into
//! an MCP `notifications/claude/channel` event so the model wakes up in
//! context.
//!
//! The server-side `CommandHandler::execute` is `unreachable!()` because
//! this command is only ever delivered from server to client — never the
//! other way around. Same pattern as rship's `ExecTargetAction`.

use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
};
use serde_json::Value;

/// Push a `notifications/claude/channel` event into the recipient's MCP
/// stream. `meta` is forwarded as the event's `meta` object after the shim
/// stringifies its values (Claude Code 2.1.x validates `meta` as
/// `Record<string,string>`).
#[myko_command]
pub struct NotifyChannel {
    pub content: String,
    #[serde(default)]
    pub meta: Value,
}

impl CommandHandler for NotifyChannel {
    fn execute(self, _ctx: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("NotifyChannel is server→client only; handled via on_command on the shim")
    }
}
