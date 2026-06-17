//! Integration test: the statusline's `RECV-OFF` warning must track the LIVE
//! parent process at render time, not a stale startup marker.
//!
//! This reproduces the resume false-positive that needed a manual
//! `/mcp reconnect marshal`: a session resumed WITH the channels flag showed
//! `⚠ marshal RECV-OFF` because the shim detected channel state once at
//! startup — racing Claude's `bg-pty-host` resume relaunch — and the
//! statusline read that stale verdict back. The fix makes the statusline
//! detect live off its OWN parent (which, by render time, is the settled
//! flagged `claude`), so the warning is correct without a reconnect.
//!
//! We exercise the REAL binary against a REAL parent process whose command
//! line we control, rather than unit-testing the pure matcher — the whole bug
//! lived in *which process* got read and *when*, which only a real spawn-path
//! test can guard. Linux-only: it relies on `/bin/sh` and `/proc`; CI's
//! `check` job runs on Linux (the Windows job only cross-compiles).
#![cfg(target_os = "linux")]

use std::io::Write;
use std::process::{Command, Stdio};

const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");
const FLAG: &[&str] = &["--dangerously-load-development-channels", "server:marshal"];
const PAYLOAD: &str = r#"{"session_id":"itest-session","workspace":{"current_dir":"/tmp"}}"#;

/// Run `marshal-shim statusline` as a CHILD of `/bin/sh`, where sh's process
/// command line carries `parent_extra` as trailing positional args. The
/// statusline therefore reads exactly those tokens as its parent cmdline when
/// it live-detects channel state.
///
/// `sh -c '<shim> statusline' sh <parent_extra...>` — note NO `exec`: sh stays
/// resident and forks the shim, so the shim's parent is sh (whose `/proc`
/// cmdline includes the positionals), not the test runner. The `<shim>
/// statusline` command string itself is flag-free, so only `parent_extra`
/// determines what the parent advertises.
fn statusline_under_parent(parent_extra: &[&str]) -> String {
    let mut args = vec![
        "-c".to_string(),
        format!("'{SHIM}' statusline"),
        "sh".to_string(), // becomes $0
    ];
    args.extend(parent_extra.iter().map(|s| s.to_string()));

    let mut child = Command::new("/bin/sh")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn /bin/sh -> marshal-shim statusline");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(PAYLOAD.as_bytes())
        .expect("write statusline payload");
    let out = child.wait_with_output().expect("wait for statusline");
    assert!(
        out.status.success(),
        "statusline exited non-zero: {:?}",
        out.status
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn warns_when_parent_lacks_the_channels_flag() {
    let out = statusline_under_parent(&[]);
    assert!(
        out.contains("RECV-OFF"),
        "parent without the channels flag must warn; statusline was: {out:?}"
    );
}

#[test]
fn silent_when_parent_has_the_channels_flag() {
    // This is the resume case: the live parent DOES carry the flag, so the
    // statusline must NOT warn — even though the historical bug would have.
    let out = statusline_under_parent(FLAG);
    assert!(
        !out.contains("RECV-OFF"),
        "parent with the channels flag must NOT warn; statusline was: {out:?}"
    );
    // Sanity: it still rendered the normal prefix (first 8 chars of the id).
    assert!(
        out.contains("itest-se"),
        "expected the session id prefix in the statusline; got: {out:?}"
    );
}
