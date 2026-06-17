//! `marshal-shim statusline` — Claude Code `statusLine` renderer.
//!
//! Reads Claude Code's JSON status payload from stdin, prints
//! `[user@host dir sid8]` to stdout, and exits. Dispatched from `main`
//! BEFORE the tokio runtime is built, so this per-render hot path never
//! pays for async / WS / MCP init — which is why it lives as a
//! subcommand of the one binary rather than a separate artifact that has
//! to be built, deployed, and kept in lockstep. Configured in Claude
//! Code as:
//!
//! ```json
//! "statusLine": { "type": "command", "command": "marshal-shim statusline" }
//! ```
//!
//! Everything in the prefix is derivable from Claude's stdin payload
//! (`workspace.current_dir`, `session_id`) plus the local environment
//! (`USER`/`USERNAME`, `gethostname`). The short session id is the first
//! 8 chars of Claude's canonical session_id, matching the shim's MCP
//! registration so the same id appears everywhere a human reads it.

use std::io::Read;

pub fn render() {
    let mut input = String::new();
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

    // Current git branch, rendered in parentheses after the path per
    // git-prompt.sh (__git_ps1) convention. None on detached HEAD / non-repo.
    let branch = git_branch(&cwd);

    // Warn when this session can't receive live peer messages (claude launched
    // without the channels flag). The shim records this per-session; absence of
    // a marker is treated as "unknown" → no false alarm.
    let recv_off = !session_id.is_empty() && crate::channels::recv_off(session_id);

    println!(
        "{}",
        format_prefix(&user, host, dir, branch.as_deref(), sid8, recv_off)
    );
}

/// Current branch of the repo at `cwd`, via the same call the shim uses for
/// `Session.git_branch`. None when detached (`HEAD`), not a repo, or git fails.
fn git_branch(cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8(out.stdout).ok()?;
    let b = b.trim();
    if b.is_empty() || b == "HEAD" {
        None
    } else {
        Some(b.to_string())
    }
}

/// Render the statusline prefix. Pure so the formatting contract is
/// testable without touching stdin / env / gethostname. `branch` is rendered
/// as ` (branch)` after the path per git-prompt.sh convention; `recv_off`
/// appends a RECV-OFF warning when this session can't receive peer messages.
fn format_prefix(
    user: &str,
    host: &str,
    dir: &str,
    branch: Option<&str>,
    sid8: &str,
    recv_off: bool,
) -> String {
    let loc = match branch {
        Some(b) => format!("{dir} ({b})"),
        None => dir.to_string(),
    };
    let base = if sid8.is_empty() {
        format!("[{user}@{host} {loc}]")
    } else {
        format!("[{user}@{host} {loc} {sid8}]")
    };
    if recv_off {
        format!("{base} ⚠ marshal RECV-OFF")
    } else {
        base
    }
}

fn extract_cwd(v: &serde_json::Value) -> Option<String> {
    v.get("workspace")
        .and_then(|w| w.get("current_dir"))
        .and_then(|s| s.as_str())
        .or_else(|| v.get("cwd").and_then(|s| s.as_str()))
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::format_prefix;

    #[test]
    fn prefix_includes_sid_when_present() {
        assert_eq!(
            format_prefix("max", "pulse-admin", "pulse-deploy", None, "5846bf98", false),
            "[max@pulse-admin pulse-deploy 5846bf98]"
        );
    }

    #[test]
    fn prefix_omits_sid_when_empty() {
        assert_eq!(
            format_prefix("max", "pulse-admin", "pulse-deploy", None, "", false),
            "[max@pulse-admin pulse-deploy]"
        );
    }

    #[test]
    fn branch_rendered_in_parens_after_dir() {
        assert_eq!(
            format_prefix(
                "max",
                "pulse-admin",
                "pulse-deploy",
                Some("feat/rotunda-mesh-partition-support"),
                "5846bf98",
                false
            ),
            "[max@pulse-admin pulse-deploy (feat/rotunda-mesh-partition-support) 5846bf98]"
        );
    }

    #[test]
    fn recv_off_appends_warning_after_branch() {
        assert_eq!(
            format_prefix(
                "max",
                "pulse-admin",
                "pulse-deploy",
                Some("main"),
                "5846bf98",
                true
            ),
            "[max@pulse-admin pulse-deploy (main) 5846bf98] ⚠ marshal RECV-OFF"
        );
    }
}
