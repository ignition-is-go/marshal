//! `ExternalAdapter` against `tests/fixtures/marshal-launch-fake`: discovery
//! on PATH, the JSON round trip, and each failure reason reaching the user.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use marshal_launch::{Adapter, ExternalAdapter, LaunchRequest, PROTOCOL_VERSION};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fake() -> ExternalAdapter {
    let path = std::env::join_paths([PathBuf::from("/nonexistent"), fixtures()]).unwrap();
    ExternalAdapter::find_in("fake", &path).unwrap()
}

fn req(options: &[&str]) -> LaunchRequest {
    LaunchRequest {
        version: PROTOCOL_VERSION,
        harness: "claude".into(),
        argv: vec!["claude".into()],
        cwd: "~/work/app".into(),
        name: "claude-app".into(),
        agent_id: "launch-6f1c".into(),
        marshal_version: marshal_launch::MARSHAL_VERSION.into(),
        env: BTreeMap::from([
            ("MARSHAL_AGENT_ID".into(), "launch-6f1c".into()),
            (
                "MARSHAL_DAEMON_ADDRESS".into(),
                "ws://marshal.mesh:6155".into(),
            ),
        ]),
        options: options.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn finds_the_adapter_on_path_and_says_when_it_is_missing() {
    let a = fake();
    assert_eq!(a.name(), "fake");
    assert_eq!(a.program(), fixtures().join("marshal-launch-fake"));
    let e = ExternalAdapter::find_in("nope", &OsString::from(fixtures())).unwrap_err();
    assert_eq!(
        e.to_string(),
        "no `nope` adapter: there is no executable marshal-launch-nope on PATH"
    );
    assert!(ExternalAdapter::find_in("../fake", &OsString::from(fixtures())).is_err());
}

#[test]
fn launch_sends_the_request_and_reads_where_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let log_opt = format!("--log={}", log.display());
    let r = req(&[&log_opt]);
    fake().check(&r).unwrap();
    let l = fake().launch(&r).unwrap();
    assert_eq!(l.handle, "fake-ok");
    assert_eq!(l.host, "fake-host");
    assert_eq!(l.cwd, "/work/app");
    assert_eq!(l.r#where, "fake-host fake:app");
    assert_eq!(l.attach.as_deref(), Some("true"));
    // The adapter got the request exactly, as one JSON object per verb.
    let logged = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<_> = logged.lines().collect();
    assert_eq!(lines.len(), 2);
    for (line, verb) in lines.iter().zip(["check", "launch"]) {
        let json = line.strip_prefix(&format!("{verb} ")).unwrap();
        assert_eq!(serde_json::from_str::<LaunchRequest>(json).unwrap(), r);
    }
}

#[test]
fn stop_and_status() {
    fake().stop("fake-ok").unwrap();
    assert_eq!(fake().status("fake-ok").unwrap(), Some(true));
    assert_eq!(fake().status("fake-gone").unwrap(), Some(false));
    // No status verb (exit 2) is "can't tell"; any other failure is an error.
    assert_eq!(fake().status("fake-no-status").unwrap(), None);
    let e = fake()
        .status("fake-fail=unreachable")
        .unwrap_err()
        .to_string();
    assert!(e.contains("status failed (exit 1)"), "{e}");
    let e = fake()
        .stop("fake-fail=unreachable")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("marshal-launch-fake stop failed (exit 1): can't reach fake-host"),
        "{e}"
    );
}

/// Each reason `check` can fail for reaches the user in the error.
#[test]
fn check_failures_carry_the_adapters_reason() {
    for (reason, says) in [
        (
            "unreachable",
            "can't reach fake-host: No route to host; nothing was started",
        ),
        ("daemon-port", "can't reach the daemon at marshal.mesh:6155"),
        (
            "hook-port",
            "can't reach the daemon's hook port at marshal.mesh:6156",
        ),
        (
            "shim-version",
            "the target's marshal-shim is 0.1.0, but this launcher is 0.60.2",
        ),
        (
            "hooks",
            "hooks on fake-host don't run `marshal-shim claude-hook`",
        ),
        ("mcp", "MCP server on fake-host is not marshal-shim"),
        (
            "daemon-address",
            "fake-host has ~/.config/marshal/daemon-address naming ws://elsewhere:6155",
        ),
    ] {
        let e = fake()
            .check(&req(&[&format!("--fail={reason}")]))
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("marshal-launch-fake check failed (exit 1): "),
            "{reason}: {e}"
        );
        assert!(e.contains(says), "{reason}: {e}");
    }
}

/// A launch that went wrong is an error that says how, never a `Launched`.
#[test]
fn launch_failures_say_how() {
    for (reason, says) in [
        (
            "unreachable",
            "launch failed (exit 1): can't reach fake-host",
        ),
        (
            "silent",
            "launch failed (exit 3): it said nothing on stderr",
        ),
        ("killed", "launch was killed before it finished"),
        (
            "not-json",
            "exited 0 but its stdout is not one JSON object: \"started it, probably\"",
        ),
        ("not-object", "answered [1,2], not a JSON object"),
        (
            "wrong-version",
            "answered in contract version 2; this launcher speaks 1",
        ),
        ("no-host", "answered without the contract's fields"),
    ] {
        let e = fake()
            .launch(&req(&[&format!("--fail={reason}")]))
            .unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains(says), "{reason}: {e}");
    }
}
