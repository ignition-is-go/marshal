//! `--help` and `--version` answer and exit without starting the daemon, and
//! an argument the daemon doesn't know is refused instead of being ignored.
//! `--foreground`, which earlier docs told people to pass, still starts it.
//!
//! Each run binds port 0 and has a deadline, so a daemon that starts when it
//! should have exited fails the test instead of hanging it.

use std::ffi::OsStr;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(20);

fn spawn<A: AsRef<OsStr>>(args: &[A]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_marshal-daemon"))
        .args(args)
        .env("MARSHAL_BIND", "127.0.0.1:0")
        .env("MARSHAL_HOOK_BIND", "127.0.0.1:0")
        .env_remove("MYKO_POSTGRES_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn marshal-daemon")
}

fn daemon<A: AsRef<OsStr> + std::fmt::Debug>(args: &[A]) -> Output {
    let mut child = spawn(args);
    let started = Instant::now();
    while child.try_wait().expect("poll marshal-daemon").is_none() {
        if started.elapsed() > DEADLINE {
            let _ = child.kill();
            panic!("marshal-daemon {args:?} kept running: it started instead of exiting");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child
        .wait_with_output()
        .expect("collect marshal-daemon output")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn version_prints_the_name_and_version_and_exits() {
    for flag in ["--version", "-V"] {
        let out = daemon(&[flag]);
        assert!(
            out.status.success(),
            "{flag}: {:?} {}",
            out.status,
            stderr(&out)
        );
        assert_eq!(
            stdout(&out),
            format!("marshal-daemon {}\n", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(stderr(&out), "", "{flag} must not start the daemon");
    }
}

#[test]
fn help_names_the_flags_and_the_environment_and_exits() {
    for flag in ["--help", "-h"] {
        let out = daemon(&[flag]);
        assert!(
            out.status.success(),
            "{flag}: {:?} {}",
            out.status,
            stderr(&out)
        );
        let help = stdout(&out);
        for expected in [
            "Usage: marshal-daemon",
            "--version",
            "MARSHAL_BIND",
            "MARSHAL_HOOK_BIND",
            "MYKO_POSTGRES_URL",
        ] {
            assert!(
                help.contains(expected),
                "{flag} output lacks {expected:?}:\n{help}"
            );
        }
        assert_eq!(stderr(&out), "", "{flag} must not start the daemon");
    }
}

#[test]
fn an_unknown_argument_is_refused_with_a_usage_error() {
    let out = daemon(&["--no-such-flag"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    let err = stderr(&out);
    assert!(err.contains("unknown argument: --no-such-flag"), "{err}");
    assert!(err.contains("marshal-daemon --help"), "{err}");
}

#[test]
fn foreground_is_accepted_and_starts_the_daemon() {
    let mut child = spawn(&["--foreground"]);
    std::thread::sleep(Duration::from_secs(2));
    let exited = child.try_wait().expect("poll marshal-daemon");
    let _ = child.kill();
    let out = child
        .wait_with_output()
        .expect("collect marshal-daemon output");
    assert!(
        exited.is_none(),
        "--foreground exited instead of serving: {}",
        stderr(&out)
    );
}

#[test]
fn an_argument_after_foreground_is_refused() {
    let out = daemon(&["--foreground", "--bind", "127.0.0.1:1"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown argument: --bind"),
        "{}",
        stderr(&out)
    );
}

#[cfg(unix)]
#[test]
fn an_argument_that_is_not_utf8_is_refused_without_a_panic() {
    use std::os::unix::ffi::OsStrExt;
    let out = daemon(&[OsStr::from_bytes(b"--\xff")]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown argument"),
        "{}",
        stderr(&out)
    );
}
