//! Integration test: the statusline's marshal warning must reflect whether the
//! session can ACTUALLY receive — i.e. whether the marshal daemon is
//! reachable — NOT whether the live-channel launch flag is present.
//!
//! History this guards against: an earlier warning keyed on the
//! `--dangerously-load-development-channels` flag, detected from the parent
//! process. That false-alarmed on every flagless session (VS Code launches a
//! bare `claude`) even though the flag-independent inbox was delivering fine,
//! and a resume variant raced the `bg-pty-host` relaunch. Delivery has two
//! paths (live channel + inbox); both require the daemon, so "can't receive"
//! means "daemon unreachable". This test drives the REAL `marshal-shim
//! statusline` binary against a REAL TCP listener (reachable) and a dead
//! address (unreachable) — no parent-process / spawn-path assumptions, which
//! is what made the previous test give false confidence.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");
const PAYLOAD: &str = r#"{"session_id":"itest-session","workspace":{"current_dir":"/tmp"}}"#;
const WARNING: &str = "marshal UNREACHABLE";

/// Run `marshal-shim statusline` with `MARSHAL_DAEMON_ADDRESS=daemon_addr` and
/// the standard status payload on stdin; return its stdout. Env is the highest-
/// priority address source, so this exercises the real reachability probe
/// without touching any on-disk config.
fn statusline_with_daemon(daemon_addr: &str) -> String {
    let mut child = Command::new(SHIM)
        .arg("statusline")
        .env("MARSHAL_DAEMON_ADDRESS", daemon_addr)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn marshal-shim statusline");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(PAYLOAD.as_bytes())
        .expect("write payload");
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_string(&mut out)
        .expect("read stdout");
    child.wait().expect("wait");
    out
}

#[test]
fn warns_when_daemon_unreachable() {
    // Port 1 on loopback: nothing listens, connect fails fast.
    let out = statusline_with_daemon("ws://127.0.0.1:1");
    assert!(
        out.contains(WARNING),
        "unreachable daemon must warn; statusline was: {out:?}"
    );
}

#[test]
fn silent_when_daemon_reachable() {
    // A real listener that accepts the probe's TCP connect — models a live
    // daemon, i.e. the inbox path works, so NO warning even though this
    // session has no live-channel flag.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe listener");
    let addr = listener.local_addr().unwrap();
    // Accept in the background so connect_timeout succeeds.
    let handle = std::thread::spawn(move || {
        // One connect from the probe is enough; ignore errors on teardown.
        let _ = listener.accept();
    });

    let out = statusline_with_daemon(&format!("ws://{addr}"));
    assert!(
        !out.contains(WARNING),
        "reachable daemon must NOT warn; statusline was: {out:?}"
    );
    // Sanity: a normal prefix rendered (cwd basename present, bracketed).
    assert!(
        out.trim_start().starts_with('[') && out.contains("tmp"),
        "expected a rendered statusline prefix; got: {out:?}"
    );
    let _ = handle.join();
}
