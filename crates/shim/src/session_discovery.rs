//! Resolve Claude Code's canonical session_id for THIS shim invocation.
//!
//! The shim is launched as Claude Code's stdio MCP child. Claude Code does
//! not pass the session_id through env or stdin, so we have to learn it
//! out-of-band. Claude Code writes a per-session transcript at:
//!
//!     ~/.claude/projects/<encoded_cwd>/<session_id>.jsonl
//!
//! …where `<session_id>` IS the canonical id (a uuid) — the same id the
//! daemon's `/hook/*` endpoints receive in their stdin payload. If the shim
//! adopts that same id, the hook-created Session row and the shim-created
//! Session row collapse to one row keyed by `session_id`. No parallel rows.
//!
//! Discovery strategy: list every `*.jsonl` under `~/.claude/projects/*/`,
//! filter to ones whose contents reference our cwd (matches `"cwd":"<cwd>"`
//! in the first ~100 lines), sort by mtime descending, take the newest.
//!
//! We deliberately don't compute Claude Code's cwd-encoding rule. The rule
//! is not a published API and varies by platform; walking the projects/
//! tree and matching on file *content* sidesteps it entirely.
//!
//! Bounded poll: the .jsonl may not exist at the moment Claude Code spawns
//! the shim, so we retry on a short interval up to `MAX_WAIT`. Hard-fail if
//! we still can't find it — better to die loudly than to register under a
//! synthetic id and have peers send messages into the void.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use marshal_entities::SessionId;

const MAX_WAIT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Cap how many lines we read per candidate when looking for the cwd
/// marker. Claude Code emits a few header lines (`last-prompt`,
/// `permission-mode`, etc.) before the first event carrying a `cwd`
/// field, but the marker shows up well within the first dozen lines.
/// 200 is comfortably above the worst case and bounds I/O if we get
/// pointed at an unusually large unrelated transcript.
const MAX_HEADER_LINES: usize = 200;

/// Resolve this shim's Claude-canonical session_id from disk. Blocks up to
/// `MAX_WAIT` waiting for Claude Code to create the transcript file.
pub fn resolve(cwd: &str) -> Option<SessionId> {
    let home = home_dir()?;
    resolve_under(&home, cwd, MAX_WAIT)
}

/// Test seam: same logic as `resolve()` but takes the HOME root and the
/// max wait explicitly. Lets unit tests exercise the real scan against a
/// synthetic `~/.claude/projects` tree.
pub(crate) fn resolve_under(home: &Path, cwd: &str, max_wait: Duration) -> Option<SessionId> {
    let projects = home.join(".claude").join("projects");
    if !projects.is_dir() {
        log::warn!(
            "[marshal-shim] session discovery: {:?} does not exist",
            projects
        );
        return None;
    }

    let cwd_norm = normalize_cwd(cwd);
    let deadline = Instant::now() + max_wait;
    loop {
        if let Some(id) = scan_once(&projects, &cwd_norm) {
            return Some(id);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn scan_once(projects: &Path, cwd: &str) -> Option<SessionId> {
    let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
    let Ok(project_dirs) = std::fs::read_dir(projects) else {
        return None;
    };
    for project_dir in project_dirs.flatten() {
        let dir_path = project_dir.path();
        if !dir_path.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir_path) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = f.metadata() else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            candidates.push((mtime, p));
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));

    for (_, path) in candidates {
        if jsonl_matches_cwd(&path, cwd)
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            log::info!(
                "[marshal-shim] session discovery: matched {} → session_id {}",
                path.display(),
                stem
            );
            return Some(SessionId(Arc::from(stem)));
        }
    }
    None
}

fn jsonl_matches_cwd(path: &Path, cwd: &str) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // Claude Code writes cwd as a JSON-string value. JSON-escape backslashes
    // (Windows) and double-quotes so the substring match is exact.
    let needle = format!("\"cwd\":\"{}\"", json_escape(cwd));
    let reader = BufReader::new(file);
    for (i, line) in reader.lines().enumerate() {
        if i >= MAX_HEADER_LINES {
            break;
        }
        let Ok(line) = line else { continue };
        if line.contains(&needle) {
            return true;
        }
    }
    false
}

/// Minimal JSON string escape — covers the characters Claude Code's path
/// values can plausibly contain (backslash, double-quote). Sufficient for
/// substring matching; not a general JSON encoder.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
}

/// Normalize a cwd for content matching. We leave the path verbatim —
/// Claude Code records `cwd` exactly as the process saw it at startup,
/// so matching on the un-resolved form is what aligns with the transcript.
fn normalize_cwd(cwd: &str) -> String {
    cwd.to_string()
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(p));
    }
    let drive = std::env::var_os("HOMEDRIVE")?;
    let path = std::env::var_os("HOMEPATH")?;
    let mut full = PathBuf::from(drive);
    full.push(path);
    Some(full)
}

#[cfg(not(any(unix, windows)))]
fn home_dir() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a synthetic ~/.claude/projects/ tree under a tempdir, write
    /// jsonl candidates with controlled mtimes, and assert the live
    /// `resolve_under` picks the right one.
    #[test]
    fn newest_jsonl_with_matching_cwd_wins() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj_a = home.join(".claude").join("projects").join("-tmp-fake-a");
        let proj_b = home.join(".claude").join("projects").join("-tmp-fake-b");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();

        write_transcript(&proj_a.join("aaa1-old.jsonl"), "/tmp/fake/a");
        std::thread::sleep(Duration::from_millis(50));
        // Newer-but-wrong-cwd → must not win.
        write_transcript(&proj_a.join("aaa2-newer-wrongcwd.jsonl"), "/tmp/fake/other");
        std::thread::sleep(Duration::from_millis(50));
        // Newest, matches our cwd, lives in a different "project" dir →
        // wins despite the encoded-dir mismatch, because we match on
        // file *content*, not directory name.
        write_transcript(&proj_b.join("bbb-newest-match.jsonl"), "/tmp/fake/a");

        let got = resolve_under(&home, "/tmp/fake/a", Duration::from_millis(10));
        assert_eq!(got.as_ref().map(|s| s.0.as_ref()), Some("bbb-newest-match"));
    }

    #[test]
    fn no_match_returns_none_after_deadline() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj = home.join(".claude").join("projects").join("-tmp-other");
        std::fs::create_dir_all(&proj).unwrap();
        write_transcript(&proj.join("ccc.jsonl"), "/tmp/different-cwd");

        let started = Instant::now();
        let got = resolve_under(&home, "/tmp/fake/a", Duration::from_millis(100));
        assert!(got.is_none());
        // Should not return before the deadline (proves the bounded
        // poll is exercising the wait path, not bailing immediately).
        assert!(started.elapsed() >= Duration::from_millis(90));
    }

    #[test]
    fn windows_style_cwd_with_backslashes_matches() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj = home.join(".claude").join("projects").join("C--Users-admin");
        std::fs::create_dir_all(&proj).unwrap();
        // The transcript JSON-escapes backslashes: "cwd":"C:\\Users\\admin"
        let mut f = std::fs::File::create(proj.join("ddd.jsonl")).unwrap();
        writeln!(f, r#"{{"type":"system","cwd":"C:\\Users\\admin","timestamp":1}}"#).unwrap();
        drop(f);

        let got = resolve_under(&home, r"C:\Users\admin", Duration::from_millis(10));
        assert_eq!(got.as_ref().map(|s| s.0.as_ref()), Some("ddd"));
    }

    #[test]
    fn missing_projects_dir_returns_none_immediately() {
        let tmp = make_tempdir();
        let home = tmp.join("nonexistent-home");
        let started = Instant::now();
        let got = resolve_under(&home, "/tmp/whatever", Duration::from_secs(5));
        assert!(got.is_none());
        // Hard-fast: no projects/ dir → bail before any polling.
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    fn write_transcript(path: &Path, cwd: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        writeln!(f, r#"{{"type":"last-prompt","leafUuid":"x"}}"#).unwrap();
        writeln!(f, r#"{{"type":"permission-mode","permissionMode":"default"}}"#).unwrap();
        let escaped = cwd.replace('\\', r"\\").replace('"', r#"\""#);
        writeln!(f, r#"{{"type":"system","cwd":"{escaped}","timestamp":1}}"#).unwrap();
    }

    fn make_tempdir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!(
            "marshal-shim-discover-{}-{nanos:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
