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
        let tmux = Tmux::builder().socket_path(socket).build()?;
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

#[derive(Debug)]
pub(super) struct OwnedGroup {
    pid: u32,
    started: String,
}

impl OwnedGroup {
    pub(super) fn capture(pid: u32) -> Result<Self> {
        Ok(Self {
            pid,
            started: Self::start_time(pid)?,
        })
    }

    fn start_time(pid: u32) -> Result<String> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let fields = stat.rsplit_once(") ").context("invalid process stat")?.1;
        Ok(fields
            .split_whitespace()
            .nth(19)
            .context("missing process start time")?
            .to_owned())
    }

    pub(super) fn argument(&self) -> String {
        format!("{}:{}", self.pid, self.started)
    }

    pub(super) fn identity(pid: u32) -> Result<String> {
        Ok(format!("{pid}:{}", Self::start_time(pid)?))
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        let (pid, started) = value
            .split_once(':')
            .context("invalid owned server group")?;
        let group = Self {
            pid: pid.parse()?,
            started: started.to_owned(),
        };
        anyhow::ensure!(
            group.pid > 1
                && Self::start_time(group.pid)? == group.started
                && unsafe { libc::getpgid(group.pid as i32) } == group.pid as i32,
            "owned server identity changed"
        );
        Ok(group)
    }
}

impl Drop for OwnedGroup {
    fn drop(&mut self) {
        let same_group = match Self::start_time(self.pid) {
            Ok(value) => value == self.started,
            Err(error) => error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        };
        if same_group {
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
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
