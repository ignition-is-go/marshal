#![cfg(unix)]

use std::{
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn codex_bridge_exits_when_its_launcher_disappears() {
    let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("parent liveness pipe");
    let reader_fd = reader.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_marshal-shim"));
    command
        .args([
            "codex-bridge",
            "--daemon",
            "ws://127.0.0.1:9",
            "--endpoint",
            "ws://127.0.0.1:9",
            "--parent-fd",
            &reader_fd.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(reader_fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut bridge = command.spawn().expect("start bridge");
    drop(reader);
    thread::sleep(Duration::from_millis(250));
    assert!(
        bridge.try_wait().expect("inspect bridge").is_none(),
        "the bridge must stay alive while its launcher owns the liveness pipe"
    );

    drop(writer);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if bridge.try_wait().expect("inspect bridge").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = bridge.kill();
            let _ = bridge.wait();
            panic!("the bridge survived after its launcher liveness pipe closed");
        }
        thread::sleep(Duration::from_millis(25));
    }
}
