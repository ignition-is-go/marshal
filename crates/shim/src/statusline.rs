//! `marshal-shim statusline` — Claude Code `statusLine` renderer.
//!
//! Reads Claude Code's JSON status payload from stdin, prints
//! `[user@host dir (branch) nickname]` to stdout, and exits. Dispatched from `main`
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
//! (`USER`/`USERNAME`, `gethostname`). The handle is the session's
//! deterministic `marshal_entities::nickname` of its canonical session_id —
//! the same name every peer computes, so it's what they address.

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

    // Memorable, deterministic session handle (adjective-noun, e.g.
    // `swift-falcon`) instead of a raw hex id prefix — peers/agents address a
    // session by this, and it must not read like a git commit hash. Empty when
    // Claude gave us no session_id (then the handle is omitted).
    let handle = if session_id.is_empty() {
        String::new()
    } else {
        // Prefer the daemon-ASSIGNED handle the shim mirrored to a per-session
        // file (what peers actually address) over the local deterministic
        // computation, which mis-routes when the daemon salted this session's
        // nickname on a collision. Falls back to the computed name if the shim
        // hasn't written it yet.
        crate::read_assigned_nickname(session_id)
            .unwrap_or_else(|| marshal_entities::nickname(session_id))
    };

    // Current git branch, rendered in parentheses after the path per
    // git-prompt.sh (__git_ps1) convention. None on detached HEAD / non-repo.
    let branch = git_branch(&cwd);

    // Warn when marshal is genuinely broken for THIS session, ordered by root
    // cause (see `pick_warning`). We deliberately do NOT warn merely because the
    // live-channel flag is absent: the flag-independent inbox still delivers for
    // flagless (e.g. VS Code) sessions whenever the daemon is reachable, so a
    // flag-based warning false-alarms on functional sessions.
    let warning = marshal_warning(session_id);

    println!(
        "{}",
        format_prefix(&user, host, dir, branch.as_deref(), &handle, warning)
    );
}

/// A session heartbeat older than this means the shim stopped writing it — the shim
/// died or hung, so this session has no working marshal MCP. ~4 missed 5s publisher
/// ticks.
const HEALTH_STALE: std::time::Duration = std::time::Duration::from_secs(20);

/// This session's statusline warning, or `None` when healthy. Reads the shim's
/// heartbeat file + probes the daemon.
fn marshal_warning(session_id: &str) -> Option<&'static str> {
    let health = if session_id.is_empty() {
        None
    } else {
        crate::read_health(session_id)
    };
    pick_warning(
        crate::channels::cannot_receive(),
        health.as_ref().map(|(s, a)| (s.as_str(), *a)),
    )
}

/// Pure warning precedence, so the policy is testable without the filesystem or
/// network. Root cause first: a dead daemon (nothing works) outranks a dead/hung
/// shim (no MCP tools) outranks an unregistered session (reads work, writes get
/// rejected). `health` is `(status_word, age_since_written)` from the heartbeat
/// file; `None` = unknown (shim hasn't written yet / older build) → no health-based
/// warning, so an unknown state never false-alarms.
fn pick_warning(
    cannot_receive: bool,
    health: Option<(&str, std::time::Duration)>,
) -> Option<&'static str> {
    if cannot_receive {
        return Some("UNREACHABLE");
    }
    match health {
        Some((_, age)) if age > HEALTH_STALE => Some("shim DOWN"),
        Some(("unregistered", _)) => Some("UNREGISTERED"),
        _ => None,
    }
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
/// as ` (branch)` after the path per git-prompt.sh convention; `handle` is the
/// session's memorable nickname (omitted when empty); `warning`, when `Some`,
/// appends ` ⚠ marshal <warning>` (marshal is broken for this session — see
/// `pick_warning`).
fn format_prefix(
    user: &str,
    host: &str,
    dir: &str,
    branch: Option<&str>,
    handle: &str,
    warning: Option<&str>,
) -> String {
    let loc = match branch {
        Some(b) => format!("{dir} ({b})"),
        None => dir.to_string(),
    };
    let base = if handle.is_empty() {
        format!("[{user}@{host} {loc}]")
    } else {
        format!("[{user}@{host} {loc} {handle}]")
    };
    match warning {
        Some(w) => format!("{base} ⚠ marshal {w}"),
        None => base,
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
    use super::{HEALTH_STALE, format_prefix, pick_warning};
    use std::time::Duration;

    #[test]
    fn prefix_includes_handle_when_present() {
        assert_eq!(
            format_prefix(
                "max",
                "pulse-admin",
                "pulse-deploy",
                None,
                "swift-falcon",
                None
            ),
            "[max@pulse-admin pulse-deploy swift-falcon]"
        );
    }

    #[test]
    fn prefix_omits_handle_when_empty() {
        assert_eq!(
            format_prefix("max", "pulse-admin", "pulse-deploy", None, "", None),
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
                "swift-falcon",
                None
            ),
            "[max@pulse-admin pulse-deploy (feat/rotunda-mesh-partition-support) swift-falcon]"
        );
    }

    #[test]
    fn warning_appended_after_branch() {
        assert_eq!(
            format_prefix(
                "max",
                "pulse-admin",
                "pulse-deploy",
                Some("main"),
                "swift-falcon",
                Some("UNREACHABLE"),
            ),
            "[max@pulse-admin pulse-deploy (main) swift-falcon] ⚠ marshal UNREACHABLE"
        );
    }

    #[test]
    fn no_warning_when_healthy_or_unknown() {
        assert_eq!(pick_warning(false, None), None);
        assert_eq!(
            pick_warning(false, Some(("ok", Duration::from_secs(3)))),
            None
        );
    }

    #[test]
    fn unreachable_outranks_health() {
        // A dead daemon is the root cause regardless of any health word.
        assert_eq!(pick_warning(true, None), Some("UNREACHABLE"));
        assert_eq!(
            pick_warning(true, Some(("unregistered", Duration::ZERO))),
            Some("UNREACHABLE")
        );
    }

    #[test]
    fn stale_heartbeat_reads_as_shim_down() {
        let stale = HEALTH_STALE + Duration::from_secs(1);
        assert_eq!(pick_warning(false, Some(("ok", stale))), Some("shim DOWN"));
        // Staleness (shim not running) outranks the last-written status word.
        assert_eq!(
            pick_warning(false, Some(("unregistered", stale))),
            Some("shim DOWN")
        );
    }

    #[test]
    fn fresh_but_absent_reads_as_unregistered() {
        assert_eq!(
            pick_warning(false, Some(("unregistered", Duration::from_secs(2)))),
            Some("UNREGISTERED")
        );
    }
}
