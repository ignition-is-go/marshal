use super::*;

#[cfg(target_os = "linux")]
pub(super) struct Evidence {
    bridge_pid: u32,
    bridge_start: u64,
    backend_pid: u32,
    backend_start: u64,
    state_file: PathBuf,
    generation: u64,
    thread: String,
    endpoint: AppServerEndpoint,
}

#[cfg(not(target_os = "linux"))]
pub(super) struct Evidence;

#[cfg(not(target_os = "linux"))]
pub(super) async fn run(_args: &[String]) -> Result<()> {
    anyhow::bail!("codex-presence-observer requires Linux process and listener ownership evidence")
}

#[cfg(target_os = "linux")]
pub(super) async fn run(args: &[String]) -> Result<()> {
    if args == ["--help"] {
        println!(
            "usage: marshal-shim codex-presence-observer --daemon WS_URL (--socket PATH | --endpoint LOOPBACK_WS_URL) --thread-id ID --bridge-pid PID --bridge-start-ticks TICKS --backend-pid PID --backend-start-ticks TICKS --launcher-state-file PATH --generation N"
        );
        return Ok(());
    }
    let mut values = HashMap::new();
    let mut args = args.iter();
    while let Some(key) = args.next() {
        anyhow::ensure!(
            [
                "--daemon",
                "--socket",
                "--endpoint",
                "--thread-id",
                "--bridge-pid",
                "--bridge-start-ticks",
                "--backend-pid",
                "--backend-start-ticks",
                "--launcher-state-file",
                "--generation"
            ]
            .contains(&key.as_str()),
            "unsupported observer option {key}"
        );
        let value = args
            .next()
            .with_context(|| format!("missing observer value for {key}"))?;
        anyhow::ensure!(
            values.insert(key.as_str(), value.as_str()).is_none(),
            "duplicate observer option {key}"
        );
    }
    let required = |key| {
        values
            .get(key)
            .copied()
            .with_context(|| format!("observer requires {key}"))
    };
    anyhow::ensure!(
        values.contains_key("--socket") != values.contains_key("--endpoint"),
        "observer requires exactly one explicit backend endpoint"
    );
    let endpoint = if let Some(path) = values.get("--socket") {
        AppServerEndpoint::Unix(fs::canonicalize(path).context("resolving backend socket")?)
    } else {
        let endpoint = required("--endpoint")?;
        loopback_address(endpoint)?;
        AppServerEndpoint::WebSocket(endpoint.into())
    };
    let evidence = Arc::new(Evidence {
        bridge_pid: required("--bridge-pid")?.parse()?,
        bridge_start: required("--bridge-start-ticks")?.parse()?,
        backend_pid: required("--backend-pid")?.parse()?,
        backend_start: required("--backend-start-ticks")?.parse()?,
        state_file: fs::canonicalize(required("--launcher-state-file")?)?,
        generation: required("--generation")?.parse()?,
        thread: required("--thread-id")?.into(),
        endpoint: endpoint.clone(),
    });
    evidence.validate()?;
    let state =
        read_launcher_state(&evidence.state_file).context("reading adopted launcher state")?;
    let environment = fs::read(format!("/proc/{}/environ", evidence.bridge_pid))?;
    let agent_id = environment.split(|byte| *byte == 0).find_map(|entry| {
        std::str::from_utf8(entry)
            .ok()?
            .strip_prefix("MARSHAL_LAUNCH_ID=")
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    });
    let mut writer = LauncherStateWriter::new(
        evidence.state_file.clone(),
        PathBuf::new(),
        Some(evidence.thread.clone()),
        agent_id,
    );
    writer.state.generation = state.generation;
    let launcher = Arc::new(tokio::sync::Mutex::new(writer));
    evidence.validate()?;
    tokio::select! {
        _ = presence::run(endpoint, required("--daemon")?.into(), launcher, Some(evidence)) => anyhow::bail!("adopted runtime evidence changed; observer stopped"),
        signal = tokio::signal::ctrl_c() => { signal?; Ok(()) },
    }
}

impl Evidence {
    #[cfg(not(target_os = "linux"))]
    pub(super) fn validate(&self) -> Result<()> {
        anyhow::bail!("adoption requires Linux")
    }

    #[cfg(target_os = "linux")]
    pub(super) fn validate(&self) -> Result<()> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        process_incarnation(self.bridge_pid, self.bridge_start)?;
        process_incarnation(self.backend_pid, self.backend_start)?;
        anyhow::ensure!(
            fs::metadata(&self.state_file)?.uid() == unsafe { libc::geteuid() },
            "launcher state belongs to another user"
        );
        let state = read_launcher_state(&self.state_file)
            .context("adopted launcher state is unavailable")?;
        anyhow::ensure!(
            state.generation == self.generation
                && (state.recovery_thread_id() == Some(self.thread.as_str())
                    || state.retained_thread_id.as_deref() == Some(self.thread.as_str())
                    || state.kept_threads.contains(&self.thread)),
            "adopted launcher selection or generation changed"
        );
        let bridge = process_arguments(self.bridge_pid)?;
        anyhow::ensure!(
            bridge.iter().any(|arg| arg == "codex-bridge"),
            "pinned bridge is not a Codex bridge"
        );
        let state_arg = bridge
            .windows(2)
            .find(|args| args[0] == "--launcher-state-file")
            .context("bridge does not own a launcher state file")?;
        anyhow::ensure!(
            fs::canonicalize(&state_arg[1])? == self.state_file,
            "bridge belongs to another launcher state"
        );
        let backend = process_arguments(self.backend_pid)?;
        anyhow::ensure!(
            backend.iter().any(|arg| arg == "app-server"),
            "endpoint owner is not a Codex app-server"
        );
        let inode = match &self.endpoint {
            AppServerEndpoint::Unix(path) => {
                anyhow::ensure!(
                    fs::metadata(path)?.file_type().is_socket(),
                    "adoption endpoint is not a Unix socket"
                );
                let table = fs::read_to_string("/proc/net/unix")?;
                table
                    .lines()
                    .find_map(|line| {
                        let fields: Vec<_> = line.split_whitespace().collect();
                        (fields.len() >= 8
                            && Path::new(fields[7]) == path
                            && fields[3] == "00010000")
                            .then(|| fields[6].to_owned())
                    })
                    .context("backend Unix listener is absent")?
            }
            AppServerEndpoint::WebSocket(endpoint) => {
                let address = loopback_address(endpoint)?;
                let IpAddr::V4(ip) = address.ip() else {
                    anyhow::bail!("adoption currently requires an IPv4 loopback listener")
                };
                let key = format!(
                    "{:08X}:{:04X}",
                    u32::from_le_bytes(ip.octets()),
                    address.port()
                );
                let table = fs::read_to_string("/proc/net/tcp")?;
                table
                    .lines()
                    .find_map(|line| {
                        let fields: Vec<_> = line.split_whitespace().collect();
                        (fields.len() > 9 && fields[1] == key && fields[3] == "0A")
                            .then(|| fields[9].to_owned())
                    })
                    .context("backend TCP listener is absent")?
            }
        };
        let expected = PathBuf::from(format!("socket:[{inode}]"));
        anyhow::ensure!(
            fs::read_dir(format!("/proc/{}/fd", self.backend_pid))?
                .filter_map(Result::ok)
                .any(|entry| fs::read_link(entry.path()).ok().as_ref() == Some(&expected)),
            "explicit endpoint is not owned by the pinned backend"
        );
        process_incarnation(self.bridge_pid, self.bridge_start)?;
        process_incarnation(self.backend_pid, self.backend_start)?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn process_incarnation(pid: u32, expected: u64) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    anyhow::ensure!(
        fs::metadata(format!("/proc/{pid}"))?.uid() == unsafe { libc::geteuid() },
        "adopted process belongs to another user"
    );
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields = stat
        .rsplit_once(") ")
        .context("malformed process identity")?
        .1;
    let actual: u64 = fields
        .split_whitespace()
        .nth(19)
        .context("missing process start ticks")?
        .parse()?;
    anyhow::ensure!(actual == expected, "adopted process incarnation changed");
    Ok(())
}

#[cfg(target_os = "linux")]
fn process_arguments(pid: u32) -> Result<Vec<String>> {
    fs::read(format!("/proc/{pid}/cmdline"))?
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8(arg.to_vec()).context("non-UTF-8 adopted process arguments"))
        .collect()
}
