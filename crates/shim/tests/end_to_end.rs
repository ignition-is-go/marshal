//! Boots a real `claude-coord-daemon` as a child process and exercises the
//! daemon protocol through `DaemonClient`, simulating two coexisting shims.

use proto::rpc::{method, InboxParams, InboxResult, RosterResult, SendMessageParams};
use shim::daemon_client::DaemonClient;
use std::sync::Arc;
use std::time::Duration;

/// RAII guard that kills + reaps a child process on Drop.
/// Ensures the daemon doesn't outlive the test even if an `assert!` panics.
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn daemon_bin() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is only set for in-package binary tests.
    // For cross-crate dev-dep tests we resolve the binary path manually
    // and ensure it's built first.
    if let Ok(e) = std::env::var("CARGO_BIN_EXE_claude-coord-daemon") {
        return e.into();
    }

    // Walk up from this crate's manifest dir to find the workspace target/.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let mut p = std::path::PathBuf::from(manifest);
    p.pop();
    p.pop(); // crates/shim → workspace root
    let target_bin = p.join("target/debug/claude-coord-daemon");

    if !target_bin.exists() {
        // Build the daemon binary. Inherit stderr so build failures are visible.
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--bin", "claude-coord-daemon"])
            .current_dir(&p)
            .status()
            .expect("running cargo build for claude-coord-daemon");
        assert!(status.success(), "cargo build of claude-coord-daemon failed");
    }
    assert!(
        target_bin.exists(),
        "daemon binary still not at {target_bin:?} after cargo build"
    );
    target_bin
}

#[tokio::test]
async fn two_clients_can_message_each_other() {
    let state = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let socket = runtime.path().join("claude-coord/sock");

    let child = std::process::Command::new(daemon_bin())
        .arg("--foreground")
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("CLAUDE_COORD_LOG", "warn")
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawning daemon");
    let _guard = ChildGuard(child);

    // Wait for socket.
    let mut tries = 0;
    loop {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            break;
        }
        tries += 1;
        if tries > 100 {
            panic!("daemon did not start");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let alice = Arc::new(DaemonClient::new(
        socket.clone(),
        1,
        "/tmp/alice".into(),
        None,
    ));
    let bob = Arc::new(DaemonClient::new(
        socket.clone(),
        2,
        "/tmp/bob".into(),
        None,
    ));

    let _ = alice.whoami().await.unwrap();
    let _ = bob.whoami().await.unwrap();

    // Roster from alice's perspective shows both.
    let r: RosterResult = alice
        .call(method::ROSTER, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(r.sessions.len(), 2);

    // Alice sends to bob; bob reads inbox.
    let _: serde_json::Value = alice
        .call(
            method::SEND_MESSAGE,
            serde_json::to_value(SendMessageParams {
                to: "bob".into(),
                body: "ping".into(),
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let inbox: InboxResult = bob
        .call(
            method::INBOX,
            serde_json::to_value(InboxParams { mark_read: true }).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].body, "ping");
    assert_eq!(inbox.messages[0].from_nick, "alice");

    // Drop of `_guard` kills + reaps the daemon.
}
