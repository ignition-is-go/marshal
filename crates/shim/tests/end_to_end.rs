//! Boots a real `claude-coord-daemon` as a child process and exercises the
//! daemon protocol through `DaemonClient`, simulating two coexisting shims.

use proto::rpc::{method, InboxParams, InboxResult, RosterResult, SendMessageParams};
use shim::daemon_client::DaemonClient;
use std::sync::Arc;
use std::time::Duration;

fn daemon_bin() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo when the test binary is in the same workspace as the bin.
    // We depend on `daemon` indirectly via dev-deps; resolve via target dir.
    let exe = std::env::var("CARGO_BIN_EXE_claude-coord-daemon").ok();
    if let Some(e) = exe {
        return e.into();
    }
    // Fallback: target/debug/claude-coord-daemon relative to the manifest dir.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let mut p = std::path::PathBuf::from(manifest);
    p.pop();
    p.pop(); // workspace root
    p.push("target/debug/claude-coord-daemon");
    p
}

#[tokio::test]
async fn two_clients_can_message_each_other() {
    let state = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let socket = runtime.path().join("claude-coord/sock");

    let mut child = std::process::Command::new(daemon_bin())
        .arg("--foreground")
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("CLAUDE_COORD_LOG", "warn")
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawning daemon");

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

    let _ = child.kill();
    let _ = child.wait();
}
