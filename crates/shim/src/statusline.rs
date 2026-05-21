//! `marshal-shim statusline` — Claude Code `statusLine` renderer.
//!
//! Reads Claude Code's JSON status payload from stdin, prints
//! `[user@host dir] nickname` to stdout, and exits. Folded into the
//! shim binary so a single declarative config —
//!
//! ```json
//! "statusLine": { "type": "command", "command": "marshal-shim statusline" }
//! ```
//!
//! — works on every platform without path expansion or per-OS shells.
//!
//! Nickname resolution piggybacks on the same PPID-keyed state file the
//! MCP server writes (see [`crate::state_file`]). The shim and this
//! subcommand are sibling subprocesses of the same Claude Code parent,
//! so both sides agree on `getppid()`.

use std::io::Read;

pub fn run() -> anyhow::Result<()> {
    let mut input = String::new();
    // stdin errors are non-fatal: Claude Code always feeds JSON in
    // practice, but if it doesn't we still want to emit *something*.
    let _ = std::io::stdin().read_to_string(&mut input);

    let cwd = extract_cwd(&input)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_default();

    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string());

    let host_full = gethostname::gethostname()
        .into_string()
        .unwrap_or_else(|_| "host".to_string());
    // Match `hostname -s` — drop the domain suffix.
    let host = host_full
        .split('.')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(&host_full);

    let dir = std::path::Path::new(&cwd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let prefix = format!("[{user}@{host} {dir}]");

    match crate::state_file::read_nickname() {
        Some(n) => println!("{prefix} {n}"),
        None => println!("{prefix}"),
    }
    Ok(())
}

fn extract_cwd(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    v.get("workspace")
        .and_then(|w| w.get("current_dir"))
        .and_then(|s| s.as_str())
        .or_else(|| v.get("cwd").and_then(|s| s.as_str()))
        .map(|s| s.to_string())
}
