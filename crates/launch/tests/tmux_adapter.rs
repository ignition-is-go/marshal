//! The built-in tmux adapter on a private tmux server: launch runs argv in the
//! canonical folder with env added, check refuses a mismatched shim and a
//! taken name, status follows the session, and stop ends it.

#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use marshal_launch::tmux::{Tmux, this_host};
use marshal_launch::{Adapter, LaunchRequest, MARSHAL_VERSION};

#[test]
fn launch_status_and_stop_on_a_private_tmux_server() {
    let Some(server) = common::PrivateTmux::new("adapter") else {
        return;
    };
    let t = Tmux::new("tmux")
        .with_socket(&server.socket)
        .with_shim(common::shim_at(MARSHAL_VERSION));
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let req = LaunchRequest {
        version: 1,
        harness: "claude".into(),
        argv: ["sh", "-c", "echo \"[$MARSHAL_AGENT_ID $PWD]\"; sleep 30"]
            .map(String::from)
            .to_vec(),
        cwd: dir.display().to_string(),
        name: "launch-test".into(),
        agent_id: "launch-t".into(),
        marshal_version: MARSHAL_VERSION.into(),
        env: [("MARSHAL_AGENT_ID".to_string(), "launch-t".to_string())].into(),
        options: vec![],
    };

    let old = Tmux::new("tmux")
        .with_socket(&server.socket)
        .with_shim(common::shim_at("0.1.0"));
    let err = old.check(&req).unwrap_err().to_string();
    assert!(
        err.contains("0.1.0") && err.contains("major.minor"),
        "{err}"
    );

    t.check(&req).unwrap();
    let l = t.launch(&req).unwrap();
    let canon = std::fs::canonicalize(&dir).unwrap();
    assert_eq!(l.handle, "launch-test");
    assert_eq!(l.host, this_host());
    assert_eq!(l.cwd, canon.display().to_string());
    assert_eq!(l.r#where, format!("{} tmux:launch-test", this_host()));
    assert!(
        l.attach
            .unwrap()
            .contains(&server.socket.display().to_string())
    );
    assert_eq!(t.status(&l.handle).unwrap(), Some(true));
    // A second launch under the same name is refused by check.
    assert!(
        t.check(&req)
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );

    let want = format!("[launch-t {}]", canon.display());
    let mut seen = String::new();
    for _ in 0..50 {
        seen = server.pane("launch-test");
        if seen.contains(&want) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(seen.contains(&want), "{seen:?}");

    t.stop(&l.handle).unwrap();
    assert_eq!(t.status(&l.handle).unwrap(), Some(false));
    // Stopping what's already gone is fine.
    t.stop(&l.handle).unwrap();
}
