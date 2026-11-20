//! A private tmux server for tests, at a socket under cargo's target temp dir.

#![allow(dead_code)]

pub mod cell;
pub mod daemon;

use std::path::PathBuf;
use std::process::{Command, Stdio};

pub struct PrivateTmux {
    pub socket: PathBuf,
}

impl PrivateTmux {
    /// `None` where there's no tmux to test with.
    pub fn new(label: &str) -> Option<Self> {
        let here = Command::new("tmux")
            .arg("-V")
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !here {
            eprintln!("skipped: no tmux here");
            return None;
        }
        let socket = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("tmux-{label}-{}", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        Some(Self { socket })
    }

    pub fn tmux(&self) -> Command {
        let mut c = Command::new("tmux");
        c.arg("-S").arg(&self.socket);
        c
    }

    pub fn sessions(&self) -> Vec<String> {
        let out = self
            .tmux()
            .args(["list-sessions", "-F", "#{session_name}"])
            .stderr(Stdio::null())
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The pane's text, wrapped lines joined.
    pub fn pane(&self, name: &str) -> String {
        let out = self
            .tmux()
            .args(["capture-pane", "-p", "-J", "-t", &format!("={name}:")])
            .stderr(Stdio::null())
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

/// Ends the server and removes its socket however the test ends.
impl Drop for PrivateTmux {
    fn drop(&mut self) {
        let _ = self
            .tmux()
            .arg("kill-server")
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// A stand-in for `marshal-shim --version` at `version`.
pub fn shim_at(version: &str) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        format!("echo marshal-shim {version}"),
    ]
}
