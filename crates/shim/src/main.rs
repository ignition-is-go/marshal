//! claude-coord-shim — stdio MCP server fronting the myko daemon.
//!
//! Almost all of the work happens inside `myko_server::mcp::McpServer`,
//! which:
//! - reads JSON-RPC over stdio,
//! - connects to a myko WebSocket server (`MYKO_ADDRESS`, default
//!   `ws://localhost:5155`),
//! - auto-exposes every registered query/report/command as an MCP tool.
//!
//! All Claude Code needs is to spawn this binary as an MCP server. We just
//! force-link the entities crate so the items get registered before we
//! start advertising tools.

use myko_server::mcp::McpServer;

fn main() -> std::io::Result<()> {
    entities::link();

    // The myko mcp server reads MYKO_ADDRESS for its target. We want
    // claude-coord's default (port 6155) to win when the user hasn't
    // explicitly opted into something else, without forcing every
    // launcher to set the env var. SAFETY: set_var is sound here because
    // we're single-threaded — main hasn't spawned anything yet.
    if std::env::var_os("MYKO_ADDRESS").is_none() {
        unsafe {
            std::env::set_var("MYKO_ADDRESS", "ws://localhost:6155");
        }
    }

    let server = McpServer::with_info("claude-coord-shim", env!("CARGO_PKG_VERSION"));

    let summary = server.summary();
    eprintln!(
        "[claude-coord-shim] starting — {} queries, {} reports, {} commands available",
        summary.queries.len(),
        summary.reports.len(),
        summary.commands.len()
    );

    server.run_stdio()
}
