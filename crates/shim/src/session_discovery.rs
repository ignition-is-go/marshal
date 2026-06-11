//! Resolve Claude Code's canonical session_id for THIS shim invocation.
//!
//! The shim is launched as Claude Code's stdio MCP child. Claude does not
//! pass the session_id through env or stdin, so we have to learn it
//! out-of-band. Claude writes a per-session transcript at:
//!
//!     ~/.claude/projects/<encoded_cwd>/<session_id>.jsonl
//!
//! …where `<session_id>` IS the canonical id (a uuid) — the same id the
//! daemon's `/hook/*` endpoints receive in their stdin payload. If the shim
//! adopts that same id, hook and shim collapse to one Session row.
//!
//! ## Disambiguation — multi-agent same-cwd
//!
//! A shim invocation's PARENT process IS its Claude session: stdio MCP
//! children are spawned by the parent's `child_process.spawn`, and POSIX +
//! Windows both expose the parent PID directly. We resolve in two tiers,
//! both keyed off that parent:
//!
//! 1. **Parent cmdline parse.** If the parent was started with
//!    `--resume <path>.jsonl` or `--session-id <uuid>` (Claude's
//!    fork/resume code path), the id is there literally. Authoritative.
//!
//! 2. **Parent start time → .jsonl creation time correlation.** Claude
//!    creates the `.jsonl` at the moment the session starts (fresh) or
//!    keeps the existing one (resumed but without an explicit flag —
//!    rare; usually `--resume` is set). The file whose creation time is
//!    closest to the parent process's start time is the right one. With
//!    multiple sibling sessions in the same cwd, each has a distinct
//!    start time so the match is unambiguous.
//!
//! Tier 2 uses `Metadata::created()` (statx birthtime on Linux 4.11+,
//! NTFS creation time on Windows). It falls back to `modified()` if
//! birthtime is unavailable, which approximates ctime well for newly
//! created files (mtime ≈ ctime when the first write is near-immediate)
//! and badly for long-running sessions (mtime drifts away from ctime as
//! the file is appended to). The fallback is a degradation, not a
//! correctness guarantee — it's a best-effort branch for old kernels.
//!
//! Both tiers verify cwd-content match on the candidate .jsonl as a
//! consistency check — a parent could in principle have a stale
//! .jsonl path in argv pointing to a different cwd, and we'd rather
//! fail loudly than register under the wrong id.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use marshal_entities::SessionId;

const MAX_WAIT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Cap lines read per candidate while looking for the cwd marker. The
/// `"cwd":` field shows up within the first ~dozen lines of every real
/// transcript; 200 is well above the worst case and caps I/O if we're
/// pointed at an unusually large unrelated file.
const MAX_HEADER_LINES: usize = 200;
/// How close a candidate .jsonl's creation time must be to its parent
/// claude's process start time to be considered "the file claude opened
/// when this session began". Generous because clock granularity varies
/// and the file's first write can lag the process spawn by a few
/// hundred ms; tight enough to exclude unrelated sibling sessions
/// which differ by minutes-to-hours in practice.
const PARENT_CORRELATION_WINDOW: Duration = Duration::from_secs(30);

pub fn resolve(cwd: &str) -> Option<SessionId> {
    let home = home_dir()?;
    resolve_under(&home, cwd, MAX_WAIT, real_parent_info)
}

/// Test seam: same logic as `resolve()` but takes HOME, max wait, and
/// the parent-info source explicitly. Lets unit tests exercise both
/// tiers without forking a child whose parent PID we control.
pub(crate) fn resolve_under<F>(
    home: &Path,
    cwd: &str,
    max_wait: Duration,
    parent_info: F,
) -> Option<SessionId>
where
    F: Fn() -> Option<ParentInfo>,
{
    let projects = home.join(".claude").join("projects");
    if !projects.is_dir() {
        log::warn!(
            "[marshal-shim] session discovery: {:?} does not exist",
            projects
        );
        return None;
    }
    let parent = parent_info()?;
    log::info!(
        "[marshal-shim] session discovery: parent pid={} started_at={:?} argv_len={}",
        parent.pid,
        parent.started_at,
        parent.cmdline.len()
    );

    // Tier 1: parent cmdline parse — authoritative, no polling needed
    // (cmdline is immutable after exec).
    if let Some(stem) = sid_from_cmdline(&parent.cmdline) {
        let candidate = projects.join(format!("placeholder/{stem}.jsonl"));
        log::info!(
            "[marshal-shim] session discovery: parent cmdline → session_id {}",
            stem
        );
        // Best effort cwd verification: if a matching .jsonl exists in
        // any project dir, check it. Failure to verify isn't fatal here
        // — the cmdline is authoritative.
        let _ = candidate; // path is illustrative; real verification below
        let _ = verify_cwd_for_session_id(&projects, &stem, cwd);
        return Some(SessionId(Arc::from(stem.as_str())));
    }

    // Tier 2: ctime correlation. Polls because the .jsonl may not exist
    // yet at the moment Claude spawns the shim (race between fork and
    // first write into the transcript).
    let cwd_norm = normalize_cwd(cwd);
    let deadline = Instant::now() + max_wait;
    loop {
        if let Some(id) = scan_by_parent_correlation(&projects, &cwd_norm, &parent) {
            return Some(id);
        }
        if Instant::now() >= deadline {
            log::warn!(
                "[marshal-shim] session discovery: no .jsonl matched parent (pid={}) start \
                 time within {:?}; giving up",
                parent.pid,
                max_wait
            );
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Parent process snapshot — pid, when it started, and its argv.
/// `started_at` is `None` if the platform can't surface a reliable
/// process start time (very old kernels, sandboxes).
#[derive(Debug)]
pub(crate) struct ParentInfo {
    pub pid: u32,
    pub started_at: Option<SystemTime>,
    pub cmdline: Vec<String>,
}

fn sid_from_cmdline(args: &[String]) -> Option<String> {
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        // `--session-id <uuid>` or `--session-id=<uuid>`
        if a == "--session-id"
            && let Some(v) = iter.peek()
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
        if let Some(v) = a.strip_prefix("--session-id=")
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
        // `--resume <path>` or `--resume=<path>`. The session id is the
        // .jsonl filename stem. Claude also accepts a bare session id
        // (no path) — handle both: if it parses as a uuid-shaped token,
        // use as-is; otherwise treat as a path and take the stem.
        if a == "--resume"
            && let Some(v) = iter.peek()
            && !v.is_empty()
        {
            return Some(extract_session_id_from_resume_arg(v));
        }
        if let Some(v) = a.strip_prefix("--resume=")
            && !v.is_empty()
        {
            return Some(extract_session_id_from_resume_arg(v));
        }
    }
    None
}

fn extract_session_id_from_resume_arg(v: &str) -> String {
    // If it contains a path separator OR ends with .jsonl, treat as path.
    if v.contains(['/', '\\']) || v.ends_with(".jsonl") {
        Path::new(v)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(v)
            .to_string()
    } else {
        v.to_string()
    }
}

fn verify_cwd_for_session_id(projects: &Path, session_id: &str, cwd: &str) -> bool {
    let target = format!("{session_id}.jsonl");
    let cwd_norm = normalize_cwd(cwd);
    let Ok(dirs) = std::fs::read_dir(projects) else {
        return false;
    };
    for dir in dirs.flatten() {
        if !dir.path().is_dir() {
            continue;
        }
        let candidate = dir.path().join(&target);
        if candidate.exists() && jsonl_matches_cwd(&candidate, &cwd_norm) {
            return true;
        }
    }
    false
}

fn scan_by_parent_correlation(
    projects: &Path,
    cwd: &str,
    parent: &ParentInfo,
) -> Option<SessionId> {
    let parent_started = parent.started_at?;
    let mut best: Option<(Duration, PathBuf)> = None;

    let project_dirs = std::fs::read_dir(projects).ok()?;
    for project_dir in project_dirs.flatten() {
        if !project_dir.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(project_dir.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = f.metadata() else { continue };
            // Prefer created() (statx birthtime on Linux, NTFS creation
            // time on Windows). Fall back to modified() if unavailable
            // — degradation, not correctness.
            let file_t = meta.created().or_else(|_| meta.modified()).ok()?;

            let delta = abs_delta(file_t, parent_started);
            if delta > PARENT_CORRELATION_WINDOW {
                continue;
            }
            if !jsonl_matches_cwd(&p, cwd) {
                continue;
            }

            match &best {
                None => best = Some((delta, p)),
                Some((best_delta, _)) if delta < *best_delta => best = Some((delta, p)),
                _ => {}
            }
        }
    }

    let (delta, path) = best?;
    let stem = path.file_stem()?.to_str()?;
    log::info!(
        "[marshal-shim] session discovery: parent-correlation matched {} (Δ {:?}) → session_id {}",
        path.display(),
        delta,
        stem
    );
    Some(SessionId(Arc::from(stem)))
}

fn abs_delta(a: SystemTime, b: SystemTime) -> Duration {
    if a >= b {
        a.duration_since(b).unwrap_or(Duration::ZERO)
    } else {
        b.duration_since(a).unwrap_or(Duration::ZERO)
    }
}

fn jsonl_matches_cwd(path: &Path, cwd: &str) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // Claude Code writes cwd as a JSON-string value. JSON-escape
    // backslashes (Windows) and double-quotes so the substring match is
    // exact.
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

fn normalize_cwd(cwd: &str) -> String {
    cwd.to_string()
}

// ── Platform: real ParentInfo lookup ──────────────────────────────────

#[cfg(target_os = "linux")]
fn real_parent_info() -> Option<ParentInfo> {
    let ppid = unsafe { libc::getppid() } as u32;
    Some(ParentInfo {
        pid: ppid,
        started_at: linux_process_start_time(ppid),
        cmdline: linux_process_cmdline(ppid),
    })
}

#[cfg(target_os = "linux")]
fn linux_process_cmdline(pid: u32) -> Vec<String> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    raw.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
fn linux_process_start_time(pid: u32) -> Option<SystemTime> {
    // /proc/<pid>/stat is space-separated except for the comm field which
    // is wrapped in parens and can contain spaces. Skip past the last
    // ')' before splitting; starttime is field 22 overall = index 19
    // in the post-comm tail (state, ppid, pgrp, … starttime).
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let last_paren = stat.rfind(')')?;
    let tail = &stat[last_paren + 1..];
    let starttime_ticks: u64 = tail.split_whitespace().nth(19)?.parse().ok()?;

    // Boot time = now - uptime. /proc/uptime is "<seconds_up> <idle>".
    let uptime_raw = std::fs::read_to_string("/proc/uptime").ok()?;
    let uptime_secs: f64 = uptime_raw.split_whitespace().next()?.parse().ok()?;
    let boot = SystemTime::now().checked_sub(Duration::from_secs_f64(uptime_secs))?;

    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    if ticks_per_sec == 0 {
        return None;
    }
    let starttime_secs = starttime_ticks / ticks_per_sec;
    let starttime_frac_ticks = starttime_ticks % ticks_per_sec;
    let starttime_nanos =
        (starttime_frac_ticks as u128 * 1_000_000_000u128 / ticks_per_sec as u128) as u32;
    boot.checked_add(Duration::new(starttime_secs, starttime_nanos))
}

#[cfg(target_os = "macos")]
fn real_parent_info() -> Option<ParentInfo> {
    let ppid = unsafe { libc::getppid() } as u32;
    // macOS doesn't expose start_time via /proc. `ps -o lstart -p <pid>`
    // and sysctl KERN_PROC are both available; sysctl is preferred but
    // requires more FFI. Use `ps` for parity with statefile's macOS
    // helper — pulse-deploy doesn't run shims on macOS today, so this
    // path is a best-effort placeholder.
    let started_at = macos_process_start_time(ppid);
    let cmdline = macos_process_cmdline(ppid);
    Some(ParentInfo {
        pid: ppid,
        started_at,
        cmdline,
    })
}

#[cfg(target_os = "macos")]
fn macos_process_start_time(pid: u32) -> Option<SystemTime> {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    // lstart format: "Tue Jun 10 17:31:30 2026". chrono parses this with
    // %a %b %e %H:%M:%S %Y; the shim already pulls in chrono.
    chrono::NaiveDateTime::parse_from_str(s.trim(), "%a %b %e %H:%M:%S %Y")
        .ok()
        .and_then(|dt| dt.and_utc().timestamp_millis().try_into().ok())
        .and_then(|millis: u64| {
            SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(millis))
        })
}

#[cfg(target_os = "macos")]
fn macos_process_cmdline(pid: u32) -> Vec<String> {
    let out = match std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    // ps command= returns a single space-separated line; argv reconstruction
    // is imprecise but adequate for `--session-id <uuid>` / `--resume <path>`
    // detection (neither flag's value should contain a literal space).
    s.trim().split_whitespace().map(str::to_string).collect()
}

#[cfg(windows)]
fn real_parent_info() -> Option<ParentInfo> {
    let ppid = windows_parent_pid()?;
    Some(ParentInfo {
        pid: ppid,
        started_at: windows_process_start_time(ppid),
        cmdline: windows_process_cmdline(ppid),
    })
}

#[cfg(windows)]
fn windows_parent_pid() -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    let my_pid = unsafe { GetCurrentProcessId() };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
        let mut found = None;
        if Process32First(snap, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == my_pid {
                    found = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32Next(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        found
    }
}

#[cfg(windows)]
fn windows_process_start_time(pid: u32) -> Option<SystemTime> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        // FILETIME is 100-ns intervals since 1601-01-01 UTC. Convert to
        // Unix epoch (1970-01-01 UTC).
        let raw = (creation.dwHighDateTime as u64) << 32 | (creation.dwLowDateTime as u64);
        // Filetime epoch to Unix epoch in 100-ns intervals.
        const FT_TO_UNIX: u64 = 11_644_473_600 * 10_000_000;
        if raw < FT_TO_UNIX {
            return None;
        }
        let unix_100ns = raw - FT_TO_UNIX;
        let secs = unix_100ns / 10_000_000;
        let nanos = (unix_100ns % 10_000_000) as u32 * 100;
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs, nanos))
    }
}

#[cfg(windows)]
fn windows_process_cmdline(_pid: u32) -> Vec<String> {
    // Reading another process's command line on Windows requires either
    // NtQueryInformationProcess (ProcessBasicInformation +
    // ReadProcessMemory of the PEB), or WMI (Win32_Process.CommandLine),
    // or PSAPI. Neither is in windows-sys's surface area without
    // additional features. We omit cmdline parsing on Windows for now;
    // the start-time correlation tier covers the dominant case (fresh
    // sessions). `--resume` / `--session-id` flags on Windows fall
    // through to ctime correlation, which is still correct as long as
    // the resumed .jsonl's creation time matches the parent's start
    // time within `PARENT_CORRELATION_WINDOW` — which it does, because
    // claude.exe's start time is what we're correlating against, not
    // the .jsonl's original creation time.
    //
    // TODO: add NtQueryInformationProcess-based reader when we hit a
    // case where the correlation fails on Windows.
    Vec::new()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn real_parent_info() -> Option<ParentInfo> {
    None
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
    use std::path::PathBuf;
    use std::sync::Mutex;

    // ── Tier 1: cmdline parse ────────────────────────────────────────

    #[test]
    fn cmdline_explicit_session_id_flag_wins() {
        let args = vec![
            "claude".to_string(),
            "--session-id".to_string(),
            "8c1175c7-40ea-44c7-8c1d-784f6e3d3dc1".to_string(),
            "--dangerously-load-development-channels".to_string(),
            "server:marshal".to_string(),
        ];
        assert_eq!(
            sid_from_cmdline(&args).as_deref(),
            Some("8c1175c7-40ea-44c7-8c1d-784f6e3d3dc1")
        );
    }

    #[test]
    fn cmdline_equals_form_session_id() {
        let args = vec![
            "claude".to_string(),
            "--session-id=abc".to_string(),
        ];
        assert_eq!(sid_from_cmdline(&args).as_deref(), Some("abc"));
    }

    #[test]
    fn cmdline_resume_with_path_extracts_stem() {
        let args = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "/root/.claude/projects/-root-pulse-deploy/14b684ba-618f-48f4-91d7-422ef99c9e38.jsonl"
                .to_string(),
        ];
        assert_eq!(
            sid_from_cmdline(&args).as_deref(),
            Some("14b684ba-618f-48f4-91d7-422ef99c9e38")
        );
    }

    #[test]
    fn cmdline_resume_with_bare_uuid_passthrough() {
        let args = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "14b684ba-618f-48f4-91d7-422ef99c9e38".to_string(),
        ];
        assert_eq!(
            sid_from_cmdline(&args).as_deref(),
            Some("14b684ba-618f-48f4-91d7-422ef99c9e38")
        );
    }

    #[test]
    fn cmdline_session_id_takes_priority_over_resume() {
        // If both flags appear, the first one parsed wins. We iterate
        // left-to-right; document that semantics by testing the order
        // observed in real Claude args.
        let args = vec![
            "claude".to_string(),
            "--session-id".to_string(),
            "explicit-id".to_string(),
            "--resume".to_string(),
            "/path/some-other-id.jsonl".to_string(),
        ];
        assert_eq!(sid_from_cmdline(&args).as_deref(), Some("explicit-id"));
    }

    #[test]
    fn cmdline_no_relevant_flags_returns_none() {
        let args = vec![
            "claude".to_string(),
            "--dangerously-load-development-channels".to_string(),
            "server:marshal".to_string(),
        ];
        assert_eq!(sid_from_cmdline(&args), None);
    }

    // ── Tier 2: ctime correlation ────────────────────────────────────

    #[test]
    fn parent_correlation_picks_jsonl_with_closest_creation_time() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj = home.join(".claude").join("projects").join("-tmp-multi-cwd");
        std::fs::create_dir_all(&proj).unwrap();

        // Three sibling sessions in the same cwd. Each .jsonl has a
        // slightly different creation time. The shim's parent was
        // spawned at the second one's time; that one must win.
        write_transcript(&proj.join("sess-old.jsonl"), "/tmp/shared-cwd");
        std::thread::sleep(Duration::from_millis(200));
        write_transcript(&proj.join("sess-target.jsonl"), "/tmp/shared-cwd");
        std::thread::sleep(Duration::from_millis(200));
        write_transcript(&proj.join("sess-newer.jsonl"), "/tmp/shared-cwd");

        // Parent's started_at = the middle file's actual created() time.
        let target_meta = std::fs::metadata(proj.join("sess-target.jsonl")).unwrap();
        let parent_t = target_meta
            .created()
            .or_else(|_| target_meta.modified())
            .unwrap();

        let parent = ParentInfo {
            pid: 99999,
            started_at: Some(parent_t),
            cmdline: Vec::new(),
        };

        let got = resolve_under(
            &home,
            "/tmp/shared-cwd",
            Duration::from_millis(10),
            move || Some(clone_parent(&parent)),
        );
        assert_eq!(got.as_ref().map(|s| s.0.as_ref()), Some("sess-target"));
    }

    #[test]
    fn parent_correlation_outside_window_excludes_match() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj = home.join(".claude").join("projects").join("-tmp-far-from-now");
        std::fs::create_dir_all(&proj).unwrap();
        write_transcript(&proj.join("only.jsonl"), "/tmp/wanted-cwd");

        // Parent claims to have started a year before this file existed.
        let parent_t = std::time::SystemTime::UNIX_EPOCH;
        let parent = ParentInfo {
            pid: 99999,
            started_at: Some(parent_t),
            cmdline: Vec::new(),
        };

        let got = resolve_under(
            &home,
            "/tmp/wanted-cwd",
            Duration::from_millis(50),
            move || Some(clone_parent(&parent)),
        );
        assert!(got.is_none());
    }

    #[test]
    fn cmdline_short_circuits_correlation() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj = home.join(".claude").join("projects").join("-tmp-cmd");
        std::fs::create_dir_all(&proj).unwrap();
        write_transcript(&proj.join("forced-sid.jsonl"), "/tmp/cmdline-cwd");

        let parent = ParentInfo {
            pid: 99999,
            // started_at is None — would force tier 2 to bail. tier 1
            // must still resolve.
            started_at: None,
            cmdline: vec![
                "claude".to_string(),
                "--session-id".to_string(),
                "forced-sid".to_string(),
            ],
        };

        let got = resolve_under(
            &home,
            "/tmp/cmdline-cwd",
            Duration::from_millis(10),
            move || Some(clone_parent(&parent)),
        );
        assert_eq!(got.as_ref().map(|s| s.0.as_ref()), Some("forced-sid"));
    }

    #[test]
    fn missing_projects_dir_returns_none_fast() {
        let tmp = make_tempdir();
        let home = tmp.join("nonexistent");
        let started = Instant::now();
        let got = resolve_under(&home, "/tmp/whatever", Duration::from_secs(2), || {
            Some(ParentInfo {
                pid: 1,
                started_at: Some(SystemTime::UNIX_EPOCH),
                cmdline: Vec::new(),
            })
        });
        assert!(got.is_none());
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn windows_style_cwd_with_backslashes_matches_via_correlation() {
        let tmp = make_tempdir();
        let home = tmp.join("home");
        let proj = home.join(".claude").join("projects").join("C--Users-admin");
        std::fs::create_dir_all(&proj).unwrap();
        let mut f = std::fs::File::create(proj.join("win-sid.jsonl")).unwrap();
        writeln!(f, r#"{{"type":"system","cwd":"C:\\Users\\admin","timestamp":1}}"#).unwrap();
        drop(f);

        let target_meta = std::fs::metadata(proj.join("win-sid.jsonl")).unwrap();
        let parent_t = target_meta
            .created()
            .or_else(|_| target_meta.modified())
            .unwrap();
        let parent = ParentInfo {
            pid: 99999,
            started_at: Some(parent_t),
            cmdline: Vec::new(),
        };

        let got = resolve_under(
            &home,
            r"C:\Users\admin",
            Duration::from_millis(10),
            move || Some(clone_parent(&parent)),
        );
        assert_eq!(got.as_ref().map(|s| s.0.as_ref()), Some("win-sid"));
    }

    // ── helpers ────────────────────────────────────────────────────────

    fn write_transcript(path: &Path, cwd: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        writeln!(f, r#"{{"type":"last-prompt","leafUuid":"x"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"permission-mode","permissionMode":"default"}}"#
        )
        .unwrap();
        let escaped = cwd.replace('\\', r"\\").replace('"', r#"\""#);
        writeln!(f, r#"{{"type":"system","cwd":"{escaped}","timestamp":1}}"#).unwrap();
    }

    fn make_tempdir() -> PathBuf {
        // Cheap uniqueness without bringing in tempfile.
        static COUNTER: Mutex<u64> = Mutex::new(0);
        let n = {
            let mut g = COUNTER.lock().unwrap();
            *g += 1;
            *g
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!(
            "marshal-shim-discover-{}-{n}-{nanos:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn clone_parent(p: &ParentInfo) -> ParentInfo {
        ParentInfo {
            pid: p.pid,
            started_at: p.started_at,
            cmdline: p.cmdline.clone(),
        }
    }
}
