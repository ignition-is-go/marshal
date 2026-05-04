//! Auto-spawn the daemon binary as a detached background process.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// If `socket` isn't accepting connections, spawn the daemon and wait for it.
pub async fn ensure_running(socket: &Path, daemon_bin: &Path) -> Result<()> {
    if probe(socket).await {
        return Ok(());
    }
    detach_spawn(daemon_bin)?;
    wait_for_socket(socket, Duration::from_secs(2)).await
}

async fn probe(socket: &Path) -> bool {
    tokio::net::UnixStream::connect(socket).await.is_ok()
}

fn detach_spawn(bin: &Path) -> Result<()> {
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::{fork, setsid, ForkResult};

    // Double-fork so the grandchild is reparented to init/PID 1.
    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            // Reap the immediate child.
            match waitpid(child, None)? {
                WaitStatus::Exited(_, 0) => Ok(()),
                other => anyhow::bail!("first fork child exited unexpectedly: {other:?}"),
            }
        }
        ForkResult::Child => {
            // In child: detach from session, fork again.
            let _ = setsid();
            match unsafe { fork() }? {
                ForkResult::Parent { .. } => {
                    // First child exits cleanly.
                    std::process::exit(0);
                }
                ForkResult::Child => {
                    // Grandchild: redirect std fds to /dev/null so it can fully detach.
                    let null = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open("/dev/null")?;
                    use std::os::unix::io::AsRawFd;
                    let fd = null.as_raw_fd();
                    let _ = nix::unistd::dup2(fd, 0);
                    let _ = nix::unistd::dup2(fd, 1);
                    let _ = nix::unistd::dup2(fd, 2);
                    drop(null);

                    use std::os::unix::process::CommandExt;
                    let err = std::process::Command::new(bin).exec();
                    eprintln!("exec failed: {err}");
                    std::process::exit(127);
                }
            }
        }
    }
}

async fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if probe(socket).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("daemon at {socket:?} did not come up within {timeout:?}")
}

/// Locate the daemon binary by name, looking in the same directory as the current exe first.
pub fn locate_daemon() -> Result<PathBuf> {
    let me = std::env::current_exe().context("getting current exe")?;
    if let Some(parent) = me.parent() {
        let candidate = parent.join("claude-coord-daemon");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    // Fall back to PATH.
    which::which("claude-coord-daemon").context("finding claude-coord-daemon on PATH")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn ensure_running_returns_ok_when_socket_already_up() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sock");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        // No daemon binary needed; ensure_running should short-circuit.
        let bogus = dir.path().join("does-not-exist");
        ensure_running(&socket, &bogus).await.unwrap();
    }

    #[tokio::test]
    #[ignore] // requires python3 on PATH; run manually if available
    async fn ensure_running_spawns_helper_binary_that_creates_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sock");
        let helper = dir.path().join("helper.sh");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\nexec python3 -c 'import socket, os, time; \
s=socket.socket(socket.AF_UNIX); s.bind(\"{}\"); s.listen(1); time.sleep(5)' \n",
                socket.display()
            ),
        )
        .unwrap();
        let mut perm = std::fs::metadata(&helper).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&helper, perm).unwrap();
        ensure_running(&socket, &helper).await.unwrap();
        assert!(socket.exists());
    }
}
