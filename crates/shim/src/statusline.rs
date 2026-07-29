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

    // Surface each marshal failure state for THIS session as its OWN distinct
    // token (see `pick_warnings`) rather than collapsing to one. Two independent
    // axes: connectivity (UNREACHABLE / shim DOWN / UNREGISTERED) and live
    // channel (`no live channel`). A fully-registered session can still be
    // flag-off — a forked/resumed launch that bypassed the wrapper — which
    // silently drops LIVE delivery even while the inbox path works; that state
    // used to be invisible, and now it warns.
    let warnings = marshal_warnings(session_id);

    println!(
        "{}",
        format_prefix(&user, host, dir, branch.as_deref(), &handle, &warnings)
    );
}

/// A session heartbeat older than this means the shim stopped writing it — the shim
/// died or hung, so this session has no working marshal MCP. ~4 missed 5s publisher
/// ticks.
const HEALTH_STALE: std::time::Duration = std::time::Duration::from_secs(20);

/// This session's statusline warning states (possibly several), empty when
/// healthy. Reads the shim's heartbeat + channel-state files and probes the
/// daemon.
fn marshal_warnings(session_id: &str) -> Vec<&'static str> {
    let (health, channels_off) = if session_id.is_empty() {
        (None, false)
    } else {
        (
            crate::read_health(session_id),
            // Only a KNOWN flag-off state warns; unknown / no file never does.
            crate::read_channels(session_id) == Some(false),
        )
    };
    pick_warnings(
        crate::channels::cannot_receive(),
        health.as_ref().map(|(s, a)| (s.as_str(), *a)),
        channels_off,
    )
}

/// Pure warning policy (testable without filesystem/network): the active
/// failure states as DISTINCT tokens across two independent axes, not one
/// lumped word.
///
/// CONNECTIVITY is a causal chain, so at most one shows (the root cause): a
/// dead daemon (`UNREACHABLE`) outranks a dead/hung shim (`shim DOWN`) outranks
/// an unregistered session (`UNREGISTERED` — reads work, writes rejected). When
/// the daemon is unreachable or the shim is down, NOTHING delivers, so the
/// live-channel axis is moot and suppressed.
///
/// LIVE CHANNEL is independent: `no live channel` when this session is flag-off
/// (can't receive LIVE pushes, only the turn-boundary inbox, so a heads-down
/// agent silently misses messages). Shown ALONGSIDE a healthy or
/// merely-`UNREGISTERED` connectivity state, since both are real then.
///
/// `health` is `(status_word, age_since_written)`; `None` = unknown → no
/// health-based warning. `channels_off` is `true` only for a known flag-off.
fn pick_warnings(
    cannot_receive: bool,
    health: Option<(&str, std::time::Duration)>,
    channels_off: bool,
) -> Vec<&'static str> {
    // Deeper connectivity breaks make the live-channel axis moot: nothing
    // delivers, so report the root cause alone.
    if cannot_receive {
        return vec!["UNREACHABLE"];
    }
    if matches!(health, Some((_, age)) if age > HEALTH_STALE) {
        return vec!["shim DOWN"];
    }
    // Shim alive + daemon reachable: registration and live-channel are separate
    // signals that can co-occur — surface each.
    let mut out = Vec::new();
    if matches!(health, Some(("unregistered", _))) {
        out.push("UNREGISTERED");
    }
    if channels_off {
        out.push("no live channel");
    }
    out
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
/// session's memorable nickname (omitted when empty); `warnings`, when
/// non-empty, appends ` ⚠ marshal <w1> · <w2> …` — each active failure state as
/// its own token (see `pick_warnings`).
fn format_prefix(
    user: &str,
    host: &str,
    dir: &str,
    branch: Option<&str>,
    handle: &str,
    warnings: &[&str],
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
    if warnings.is_empty() {
        base
    } else {
        format!("{base} ⚠ marshal {}", warnings.join(" · "))
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
    use super::{HEALTH_STALE, format_prefix, pick_warnings};
    use std::time::Duration;

    fn ok_fresh() -> Option<(&'static str, Duration)> {
        Some(("ok", Duration::from_secs(3)))
    }

    #[test]
    fn prefix_includes_handle_when_present() {
        assert_eq!(
            format_prefix("max", "pulse-admin", "pulse-deploy", None, "swift-falcon", &[]),
            "[max@pulse-admin pulse-deploy swift-falcon]"
        );
    }

    #[test]
    fn prefix_omits_handle_when_empty() {
        assert_eq!(
            format_prefix("max", "pulse-admin", "pulse-deploy", None, "", &[]),
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
                &[],
            ),
            "[max@pulse-admin pulse-deploy (feat/rotunda-mesh-partition-support) swift-falcon]"
        );
    }

    #[test]
    fn single_warning_appended_after_branch() {
        assert_eq!(
            format_prefix(
                "max",
                "pulse-admin",
                "pulse-deploy",
                Some("main"),
                "swift-falcon",
                &["UNREACHABLE"],
            ),
            "[max@pulse-admin pulse-deploy (main) swift-falcon] ⚠ marshal UNREACHABLE"
        );
    }

    #[test]
    fn multiple_warnings_render_as_distinct_tokens() {
        assert_eq!(
            format_prefix(
                "max",
                "pulse-admin",
                "pulse-deploy",
                None,
                "swift-falcon",
                &["UNREGISTERED", "no live channel"],
            ),
            "[max@pulse-admin pulse-deploy swift-falcon] ⚠ marshal UNREGISTERED · no live channel"
        );
    }

    #[test]
    fn no_warnings_when_healthy_or_unknown() {
        assert!(pick_warnings(false, None, false).is_empty());
        assert!(pick_warnings(false, ok_fresh(), false).is_empty());
    }

    #[test]
    fn unreachable_outranks_and_suppresses_channel_axis() {
        // Dead daemon = nothing delivers, so the live-channel axis is moot.
        assert_eq!(pick_warnings(true, None, true), vec!["UNREACHABLE"]);
        assert_eq!(
            pick_warnings(true, Some(("unregistered", Duration::ZERO)), true),
            vec!["UNREACHABLE"]
        );
    }

    #[test]
    fn stale_heartbeat_reads_as_shim_down_and_suppresses_channel() {
        let stale = HEALTH_STALE + Duration::from_secs(1);
        assert_eq!(pick_warnings(false, Some(("ok", stale)), false), vec!["shim DOWN"]);
        // Shim down = no MCP at all; channel state is moot.
        assert_eq!(pick_warnings(false, Some(("ok", stale)), true), vec!["shim DOWN"]);
    }

    #[test]
    fn channels_off_warns_independently_when_otherwise_healthy() {
        assert_eq!(pick_warnings(false, ok_fresh(), true), vec!["no live channel"]);
    }

    #[test]
    fn unregistered_and_channels_off_co_occur_as_distinct_states() {
        assert_eq!(
            pick_warnings(false, Some(("unregistered", Duration::from_secs(2))), true),
            vec!["UNREGISTERED", "no live channel"]
        );
    }

    #[test]
    fn unregistered_alone_when_channel_on() {
        assert_eq!(
            pick_warnings(false, Some(("unregistered", Duration::from_secs(2))), false),
            vec!["UNREGISTERED"]
        );
    }
}
