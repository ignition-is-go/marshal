use super::*;
use std::os::unix::process::CommandExt;
use tmux_session::{DetachOutcome, OwnedLookup, OwnedSession, OwnershipMarker, SessionName, Tmux};

pub(super) struct ManagedContext {
    tmux: Tmux,
    runtime: tmux_session::Runtime,
    target: OwnedSession,
}

impl ManagedContext {
    pub(super) fn discover() -> Result<Option<Self>> {
        let Some(key) = std::env::var("MARSHAL_LAUNCH_KEY")
            .ok()
            .filter(|s| !s.is_empty())
        else {
            return Ok(None);
        };
        let Some(tmux_env) = std::env::var("TMUX").ok() else {
            return Ok(None);
        };
        anyhow::ensure!(
            key.starts_with("marshal-")
                && key.len() <= 64
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid managed tmux launch key"
        );
        let mut parts = tmux_env.rsplitn(3, ',');
        let session = parts.next().context("missing tmux session")?;
        let _server_pid = parts.next().context("missing tmux server PID")?;
        let socket = parts.next().context("missing tmux socket")?;
        let tmux = Tmux::builder()
            .executable(std::env::var_os("MARSHAL_TMUX_BIN").unwrap_or_else(|| "tmux".into()))
            .socket_path(socket)
            .build()?;
        let runtime = tmux_session::Runtime::new()?;
        let target = OwnedSession::new(
            SessionName::new(&key)?,
            OwnershipMarker::new("MARSHAL_LAUNCH_KEY", &key)?,
        );
        match runtime.block_on(tmux.find_owned(&target))? {
            OwnedLookup::Found(found) if found.id.to_string() == format!("${session}") => {
                let pane = runtime
                    .block_on(tmux.active_pane(found.id))?
                    .context("managed tmux pane missing")?;
                anyhow::ensure!(
                    pane.pid == Some(std::process::id())
                        && unsafe { libc::getpgrp() } == std::process::id() as i32,
                    "managed Codex must own the marked tmux pane process group"
                );
                Ok(Some(Self {
                    tmux,
                    runtime,
                    target,
                }))
            }
            _ => anyhow::bail!("current tmux session does not own this Marshal launch"),
        }
    }

    pub(super) fn detach(&self) -> Result<()> {
        match self
            .runtime
            .block_on(self.tmux.detach_owned(&self.target))?
        {
            DetachOutcome::Detached => Ok(()),
            DetachOutcome::Gone => anyhow::bail!("managed tmux session disappeared"),
            DetachOutcome::Mismatch => anyhow::bail!("managed tmux ownership changed"),
        }
    }

    pub(super) fn attached(&self) -> Result<bool> {
        match self.runtime.block_on(self.tmux.find_owned(&self.target))? {
            OwnedLookup::Found(found) => Ok(found.attached_clients > 0),
            _ => anyhow::bail!("managed tmux session no longer owned"),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct GroupIdentity {
    pid: u32,
    started: String,
}

impl GroupIdentity {
    fn stat(pid: u32) -> Result<Vec<String>> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        Ok(stat
            .rsplit_once(") ")
            .context("invalid process stat")?
            .1
            .split_whitespace()
            .map(str::to_owned)
            .collect())
    }

    fn capture(pid: u32) -> Result<Self> {
        Ok(Self {
            pid,
            started: Self::stat(pid)?
                .get(19)
                .context("missing process start time")?
                .clone(),
        })
    }

    pub(super) fn argument(&self) -> String {
        format!("{}:{}", self.pid, self.started)
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        let (pid, started) = value
            .split_once(':')
            .context("invalid owned server group")?;
        let identity = Self {
            pid: pid.parse()?,
            started: started.to_owned(),
        };
        anyhow::ensure!(
            identity.pid > 1
                && !identity.started.is_empty()
                && identity.started.bytes().all(|b| b.is_ascii_digit()),
            "invalid owned group identity"
        );
        Ok(identity)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            Self::capture(self.pid)?.started == self.started
                && unsafe { libc::getpgid(self.pid as i32) } == self.pid as i32,
            "owned process group identity changed"
        );
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct OwnedGroup(GroupIdentity);

impl OwnedGroup {
    pub(super) fn capture(pid: u32) -> Result<Self> {
        let identity = GroupIdentity::capture(pid)?;
        identity.validate()?;
        Ok(Self(identity))
    }

    pub(super) fn argument(&self) -> String {
        self.0.argument()
    }

    pub(super) fn identity(pid: u32) -> Result<String> {
        Ok(GroupIdentity::capture(pid)?.argument())
    }
}

pub(super) fn guard_runtime(groups: &[GroupIdentity]) -> Result<Vec<OwnedGroup>> {
    anyhow::ensure!(
        groups.len() == 2,
        "managed bridge requires server and owner groups"
    );
    let server = &groups[0];
    let owner = &groups[1];
    anyhow::ensure!(
        owner.pid == unsafe { libc::getppid() } as u32 && server.pid != owner.pid,
        "managed bridge owner is not its parent"
    );
    anyhow::ensure!(
        GroupIdentity::stat(server.pid)?
            .get(1)
            .and_then(|pid| pid.parse::<u32>().ok())
            == Some(owner.pid),
        "managed server is not an owner child"
    );
    for identity in groups {
        identity.validate()?;
    }
    Ok(groups.iter().cloned().map(OwnedGroup).collect())
}

impl Drop for OwnedGroup {
    fn drop(&mut self) {
        let same_group = match GroupIdentity::capture(self.0.pid) {
            Ok(identity) => identity.started == self.0.started,
            Err(error) => error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        };
        if same_group {
            unsafe {
                libc::kill(-(self.0.pid as i32), libc::SIGKILL);
            }
        }
    }
}

pub(super) fn process_group(command: &mut Command) {
    command.process_group(0);
}

pub(super) fn wait_frontend(
    frontend: &mut Child,
    bridge: &mut Child,
) -> Result<std::process::ExitStatus> {
    loop {
        if let Some(status) = bridge.try_wait()? {
            let _ = frontend.kill();
            let _ = frontend.wait();
            anyhow::bail!("managed Codex bridge exited: {status}");
        }
        if let Some(status) = frontend.try_wait()? {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub(super) fn wait_attachment(
    context: &ManagedContext,
    bridge: &mut Child,
    server: &mut Child,
) -> Result<()> {
    loop {
        anyhow::ensure!(
            bridge.try_wait()?.is_none(),
            "managed Codex bridge exited while detached"
        );
        anyhow::ensure!(
            server.try_wait()?.is_none(),
            "managed Codex app-server exited while detached"
        );
        if context.attached()? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    fn sleeping_group() -> Child {
        Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .unwrap()
    }

    fn live(pid: u32) -> bool {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .is_some_and(|stat| !stat.rsplit_once(") ").unwrap().1.starts_with('Z'))
    }

    #[test]
    fn guard_cleans_a_group_after_its_leader_dies_and_preserves_foreign_group() {
        let mut foreign = sleeping_group();
        let mut leader = Command::new("sh")
            .args(["-c", "sleep 300 & printf '%s\\n' \"$!\"; wait"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let guard = OwnedGroup::capture(leader.id()).unwrap();
        let mut child_pid = String::new();
        BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut child_pid)
            .unwrap();
        let child_pid: u32 = child_pid.trim().parse().unwrap();
        leader.kill().unwrap();
        leader.wait().unwrap();
        assert!(live(child_pid));
        drop(guard);
        let deadline = Instant::now() + Duration::from_secs(2);
        while live(child_pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!live(child_pid));
        assert!(foreign.try_wait().unwrap().is_none());
        foreign.kill().unwrap();
        foreign.wait().unwrap();
    }

    #[test]
    fn guard_rejects_a_changed_process_identity() {
        let mut child = sleeping_group();
        let mut guard = OwnedGroup::capture(child.id()).unwrap();
        guard.0.started.push('0');
        drop(guard);
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn malformed_bridge_arguments_do_not_arm_group_cleanup() {
        let mut child = sleeping_group();
        let identity = GroupIdentity::capture(child.id()).unwrap().argument();
        let result = super::super::parse_args(&[
            "--owned-group".to_owned(),
            identity,
            "--unknown".to_owned(),
        ]);
        assert!(result.is_err());
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
