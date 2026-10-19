//! `--version` and `--help` answer and exit before the TUI touches the
//! terminal or the daemon.

use std::process::{Command, Output, Stdio};

fn tui(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_marshal-tui"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run marshal-tui")
}

#[test]
fn version_prints_the_name_and_version_and_exits() {
    for flag in ["--version", "-V"] {
        let out = tui(&[flag]);
        assert!(out.status.success(), "{flag}: {:?}", out.status);
        // Trim the line ending so the check holds on Windows too.
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.trim_end(),
            format!("marshal-tui {}", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn help_lists_the_version_flag() {
    let out = tui(&["--help"]);
    assert!(out.status.success(), "{:?}", out.status);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--version"), "{help}");
}
