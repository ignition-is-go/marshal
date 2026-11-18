//! The unattended first task, with a real Claude and nobody at the keyboard.
//!
//! A launched Claude has no channels flag (adapter rule 1 in docs/agent-launcher.md adds no flags), so its mail
//! arrives through its hooks: `marshal-shim claude-hook wait` holds a
//! long-poll that wakes the idle session when a message comes. This test
//! launches a real `claude` through the built-in tmux adapter, lets
//! `marshal_launch::launch` send the first task, and never types into the
//! pane. It passes when:
//! - the session registers through the real hook listener (the launcher
//!   joins it; nothing in this test registers a row);
//! - the session marks the first task read;
//! - the session does what the task says, which needs no tool: its reply
//!   shows in the tmux pane.
//!
//! The daemon is a real one via `common::daemon::RealDaemon`
//! (`MARSHAL_E2E_DAEMON` names a binary built from this checkout), in memory, on 127.0.0.1 test
//! ports, never 6155/6156.
//!
//! Wiring: a throwaway folder under `~/src` (already trusted by Claude, so
//! the trust prompt, which we never bypass, doesn't stop it) gets a
//! `.claude/settings.json` with the marshal-shim hooks, running
//! `MARSHAL_E2E_SHIM` by absolute path. It has no `.mcp.json`: a project MCP
//! server would ask for approval, which we don't pre-answer. So the marshal
//! MCP wiring a real target has (see the design's prerequisites) is not
//! exercised here. The session also loads the operator's own user-wide
//! settings and hooks, and their Claude login. The folder is removed and
//! the session killed at the end, pass or fail.
//!
//! It is `#[ignore]`d: it starts a real Claude and spends model tokens. Run
//! explicitly, a missing prerequisite is a failure, not a skip.
//!
//! ```text
//! MARSHAL_E2E_DAEMON=<marshal-daemon binary> \
//! MARSHAL_E2E_SHIM=<a marshal-shim of this tree's major.minor> \
//! cargo test -p marshal-launch --test first_task_e2e -- --ignored --nocapture
//! ```
//!
//! - `MARSHAL_E2E_PARENT`: where to make the throwaway folder (default
//!   `~/src`). It must be trusted by Claude.
//! - `MARSHAL_E2E_TIMEOUT_SECS`: how long the agent has to act (default 300).

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::daemon::RealDaemon;
use hyphae::Gettable;
use marshal_entities::GetAllMessageReads;
use marshal_launch::Plan;
use marshal_launch::join::Roster;
use marshal_launch::tmux::Tmux;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// The throwaway project folder, removed however the test ends.
struct Folder(PathBuf);

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The hooks the marshal-shim plugin ships, running `shim` by absolute path.
fn hooks(shim: &Path) -> String {
    let s = shim.display().to_string();
    let cmd = |verb: &str| {
        serde_json::json!({
            "type": "command",
            "command": format!("{s} claude-hook {verb}"),
            "timeout": 10,
        })
    };
    let wait = serde_json::json!({
        "type": "command",
        "command": format!("{s} claude-hook wait"),
        "asyncRewake": true,
        "timeout": 2592000,
    });
    serde_json::json!({ "hooks": {
        "SessionStart": [{ "hooks": [cmd("session-start"), wait.clone()] }],
        "Stop": [{ "hooks": [wait] }],
        "UserPromptSubmit": [{ "hooks": [cmd("prompt-submit")] }],
        "PostToolUse": [{ "hooks": [cmd("post-tool-use")] }],
    }})
    .to_string()
}

#[test]
#[ignore = "starts a real Claude and spends tokens; needs MARSHAL_E2E_DAEMON and MARSHAL_E2E_SHIM (see the module docs)"]
fn an_unattended_claude_wakes_for_its_first_task_and_acts_on_it() {
    let shim = PathBuf::from(
        env("MARSHAL_E2E_SHIM")
            .expect("set MARSHAL_E2E_SHIM to a marshal-shim of this tree's version"),
    );
    let shim = std::fs::canonicalize(&shim).expect("MARSHAL_E2E_SHIM must exist");
    let act_within = Duration::from_secs(
        env("MARSHAL_E2E_TIMEOUT_SECS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(300),
    );

    // The tmux server, and so the agent, inherits this process's
    // environment. Don't hand it the running session's Claude identity.
    for (k, _) in std::env::vars() {
        if k == "CLAUDECODE"
            || k.starts_with("CLAUDE_CODE_")
            || k == "CLAUDE_PID"
            || k == "CLAUDE_EFFORT"
            || k == "TMUX"
        {
            // SAFETY: this binary's only test changes the environment
            // before it starts any thread of its own.
            unsafe { std::env::remove_var(&k) };
        }
    }

    let d = RealDaemon::from_env().expect("set MARSHAL_E2E_DAEMON to a marshal-daemon binary");
    // The shim derives the hook port (6156) from the address unless told.
    // SAFETY: as above; the daemon is a child process, not a thread here.
    unsafe { std::env::set_var("MARSHAL_BASE_URL", &d.hook_url) };

    let parent = PathBuf::from(
        env("MARSHAL_E2E_PARENT").unwrap_or_else(|| format!("{}/src", env("HOME").expect("HOME"))),
    );
    let folder = Folder(parent.join(format!(".marshal-first-task-e2e-{}", std::process::id())));
    std::fs::create_dir_all(folder.0.join(".claude")).unwrap();
    let config = folder.0.join("marshal-config");
    std::fs::create_dir_all(config.join("marshal")).unwrap();
    std::fs::write(config.join("marshal/daemon-address"), &d.address).unwrap();
    // SAFETY: the single fixture sets its environment before starting threads.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", config) };
    std::fs::write(folder.0.join(".claude/settings.json"), hooks(&shim)).unwrap();
    let cwd = std::fs::canonicalize(&folder.0).unwrap();

    // Declared after the folder, so it drops first: the session is killed
    // before its folder goes.
    let server = common::PrivateTmux::new("first-task").expect("needs tmux");
    let tmux = Tmux::new("tmux")
        .with_socket(&server.socket)
        .with_shim(vec![shim.display().to_string()]);
    let name = format!("first-task-e2e-{}", std::process::id());
    let pane = || {
        let sessions = server.sessions();
        let Some(handle) = sessions.first() else {
            return String::new();
        };
        assert_eq!(
            sessions.len(),
            1,
            "expected one launched private tmux session"
        );
        let out = server
            .tmux()
            .args([
                "capture-pane",
                "-p",
                "-J",
                "-S",
                "-500",
                "-t",
                &format!("={handle}:"),
            ])
            .stderr(Stdio::null())
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // In the task, lowercase; the answer it asks for, uppercase. So the
    // answer showing in the pane is the agent's, not the task echoed.
    let token = format!(
        "launchok-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let answer = token.to_uppercase();
    let task = format!(
        "This is an automated test of unattended task delivery. Reply with this token \
         in capital letters, and nothing else: {token}"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let roster = Roster::connect(&d.address)
            .await
            .expect("connect to the daemon");
        let reads = roster
            .client()
            .watch_query::<GetAllMessageReads>(GetAllMessageReads {});
        let plan = Plan {
            harness: "claude".into(),
            cwd: cwd.display().to_string(),
            name: name.clone(),
            launch_address: d.address.clone(),
            daemon_address: d.address.clone(),
            timeout: Duration::from_secs(120),
            check: true,
            options: vec![],
            task: Some(task),
            records: None,
        };
        let outcome = match marshal_launch::launch(&tmux, &plan).await {
            Ok(o) => o,
            Err(e) => panic!(
                "launch failed: {e:#}\nThe agent's pane:\n{}\nDaemon log:\n{}",
                pane(),
                d.log_text()
            ),
        };
        let agent = outcome.joined.session_id.clone();
        let task_id = outcome
            .task_message_id
            .clone()
            .expect("the first task was sent");
        eprintln!(
            "joined through the hook listener as {agent} ({}); first task {task_id}",
            outcome.joined.nickname
        );

        // Nobody types: the waiter has to wake the idle session.
        let started = Instant::now();
        let mut read = false;
        loop {
            read = read
                || reads.get().iter().any(|r| {
                    r.message_id.0.as_ref() == task_id && r.session_id.0.as_ref() == agent
                });
            if read && pane().contains(&answer) {
                break;
            }
            if started.elapsed() > act_within {
                panic!(
                    "within {}s: task marked read: {read}; `{answer}` in the pane: false.\n\
                     The agent's pane:\n{}",
                    act_within.as_secs(),
                    pane()
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        eprintln!("the first task was marked read, and the agent answered {answer} in its pane");
    });
    // `server` (the session), then `folder`, drop here, pass or fail.
}
