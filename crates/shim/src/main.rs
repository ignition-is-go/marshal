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
