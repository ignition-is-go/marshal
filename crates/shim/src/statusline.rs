//! `marshal-shim statusline` — Claude Code `statusLine` renderer.
//!
//! Reads Claude Code's JSON status payload from stdin, prints
//! `[user@host dir sid8]` to stdout, and exits. Folded into the shim
//! binary so a single declarative config —
//!
//! ```json
//! "statusLine": { "type": "command", "command": "marshal-shim statusline" }
//! ```
//!
//! — works on every platform without path expansion or per-OS shells.
//!
//! No daemon round-trip and no local state file: everything in the
//! rendered prefix is derivable from Claude Code's stdin payload
//! (`workspace.current_dir`, `session_id`) plus the local environment
//! (`USER`, `gethostname`). The short session id is the first 8 chars
//! of Claude's canonical session_id, matching the shim's MCP
//! registration so the same id appears everywhere a human reads it.

use std::io::Read;

pub fn run() -> anyhow::Result<()> {
    let mut input = String::new();
    // stdin errors are non-fatal: Claude Code always feeds JSON in
    // practice, but if it doesn't we still want to emit *something*.
    let _ = std::io::stdin().read_to_string(&mut input);

    let payload: Option<serde_json::Value> = serde_json::from_str(input.trim()).ok();

    let cwd = payload
        .as_ref()
        .and_then(extract_cwd)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_default();

    let session_id = payload
        .as_ref()
        .and_then(|v| v.get("session_id"))
        .and_then(|s| s.as_str())
        .unwrap_or("");

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

    let sid8 = &session_id[..session_id.len().min(8)];

    // Read the parent claude.exe's argv to detect whether marshal is in
    // its `--channels` / `--dangerously-load-development-channels` list.
    // Without that, `notifications/claude/channel` pushes are silently
    // dropped — flag it loudly so the operator notices on every render
    // instead of after the first missed peer message.
    let degraded = !crate::channel_grant::marshal_channel_granted();

    // Yellow ANSI on the warning so it actually catches the eye in the
    // status bar. Claude Code renders the statusLine command's stdout
    // verbatim and honors ANSI escapes.
    let suffix = if degraded {
        " \x1b[33m⚠ marshal channels disabled\x1b[0m"
    } else {
        ""
    };

    if sid8.is_empty() {
        println!("[{user}@{host} {dir}]{suffix}");
    } else {
        println!("[{user}@{host} {dir} {sid8}]{suffix}");
    }
    Ok(())
}

fn extract_cwd(v: &serde_json::Value) -> Option<String> {
    v.get("workspace")
        .and_then(|w| w.get("current_dir"))
        .and_then(|s| s.as_str())
        .or_else(|| v.get("cwd").and_then(|s| s.as_str()))
        .map(|s| s.to_string())
}
