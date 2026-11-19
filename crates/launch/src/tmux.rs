#[cfg(unix)]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::{process::Command, time::Duration};

#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail, ensure};

#[cfg(unix)]
use crate::external::DAEMON_CREDENTIAL_ENV;
#[cfg(unix)]
use crate::external::run_bounded;
use crate::{LaunchBackend, LaunchRequest, Launched, StopReceipt};

/// The longest tmux session name the adapter makes.
pub const MAX_NAME: usize = 64;

/// The built-in adapter. `program` is the tmux binary (`MARSHAL_TMUX_BIN`, or `tmux`).
#[derive(Debug, Clone)]
pub struct Tmux {
    program: String,
    /// A private server's socket path (`tmux -S <path>`), for tests; `None` is the user's own.
    socket: Option<PathBuf>,
    /// The command whose `--version` is checked (`MARSHAL_SHIM_BIN`, or `marshal-shim`).
    shim: Vec<String>,
    /// Environment for the tmux client itself, which a private server it
    /// starts inherits; for tests. Never passed to the agent through `-e`.
    client_env: Vec<(String, String)>,
}

impl Default for Tmux {
    fn default() -> Self {
        Self::new(std::env::var("MARSHAL_TMUX_BIN").unwrap_or_else(|_| "tmux".into()))
    }
}

impl Tmux {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            socket: None,
            shim: vec![std::env::var("MARSHAL_SHIM_BIN").unwrap_or_else(|_| "marshal-shim".into())],
            client_env: vec![],
        }
    }

    /// Set a variable in the tmux client's environment, for tests: a private
    /// server started by this adapter inherits it (`PATH`, to find a stub harness).
    pub fn with_client_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.client_env.push((key.into(), value.into()));
        self
    }

    /// Check this command's `--version` instead of `marshal-shim`'s, for tests.
    pub fn with_shim(mut self, argv: Vec<String>) -> Self {
        self.shim = argv;
        self
    }

    /// Use the private tmux server at socket path `socket` instead of the user's own.
    pub fn with_socket(mut self, socket: impl Into<PathBuf>) -> Self {
        self.socket = Some(socket.into());
        self
    }

    #[cfg(unix)]
    fn with_server<T: Send>(
        &self,
        operation: impl FnOnce(&agent_tmux::Tmux, &agent_tmux::Runtime) -> Result<T> + Send,
    ) -> Result<T> {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = agent_tmux::Runtime::new()?;
                    let mut builder = agent_tmux::Tmux::builder()
                        .executable(OsString::from(&self.program))
                        .limits(agent_tmux::Limits {
                            timeout: Duration::from_secs(5),
                            stdout_bytes: 1024 * 1024,
                            stderr_bytes: 1024 * 1024,
                            rows: 4096,
                        });
                    if let Some(socket) = &self.socket {
                        builder = builder.socket_path(socket.clone());
                    }
                    for (key, value) in &self.client_env {
                        if !DAEMON_CREDENTIAL_ENV.contains(&key.as_str()) {
                            builder = builder.client_env(key, value);
                        }
                    }
                    for key in DAEMON_CREDENTIAL_ENV {
                        builder = builder.client_env_remove(key);
                    }
                    let server = builder.client_env("LC_ALL", "C").build()?;
                    operation(&server, &runtime)
                })
                .join()
                .map_err(|_| anyhow::anyhow!("tmux operation thread panicked"))?
        })
    }

    #[cfg(unix)]
    fn observation(
        &self,
        req: &LaunchRequest,
        handle: String,
        cwd: String,
        attach: Command,
    ) -> Launched {
        let host = req
            .env
            .get("MARSHAL_HOST")
            .filter(|h| !h.trim().is_empty())
            .cloned()
            .unwrap_or_else(this_host);
        let attach = std::iter::once(attach.get_program())
            .chain(attach.get_args())
            .map(|part| format!("'{}'", part.to_string_lossy().replace('\'', r#"'"'"'"#)))
            .collect::<Vec<_>>()
            .join(" ");
        Launched {
            handle,
            r#where: format!("{host} tmux:{}", req.name),
            host,
            cwd,
            attach: Some(attach),
        }
    }
}

#[cfg(unix)]
async fn owned_session(
    server: &agent_tmux::Tmux,
    handle: &str,
) -> Result<Option<agent_tmux::SessionSummary>> {
    match server.find_owned(&owned_target(handle)?).await? {
        agent_tmux::OwnedLookup::Absent => Ok(None),
        agent_tmux::OwnedLookup::Found(session) => Ok(Some(session)),
        agent_tmux::OwnedLookup::Mismatch => {
            bail!("tmux session {handle} does not identify this launch")
        }
    }
}

#[cfg(unix)]
fn owned_target(handle: &str) -> Result<agent_tmux::OwnedSession> {
    ensure!(
        handle.starts_with("marshal-") && valid_name(handle),
        "invalid tmux recovery handle"
    );
    Ok(agent_tmux::OwnedSession::new(
        agent_tmux::SessionName::new(handle)?,
        agent_tmux::OwnershipMarker::new("MARSHAL_LAUNCH_KEY", handle)?,
    ))
}

/// The process key is scoped to one launch. A display label is reusable.
pub fn session_key(agent_id: &str) -> Result<String> {
    let key = format!("marshal-{agent_id}");
    ensure!(
        !agent_id.is_empty() && valid_name(&key),
        "invalid agent ID for a tmux launch"
    );
    Ok(key)
}

/// tmux takes `.` and `:` in a target as window and pane separators, so a name
/// is kept to letters, digits, `-` and `_`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `<harness>-<directory name>`, with anything tmux can't take turned into `-`.
pub fn default_name(harness: &str, cwd: &Path) -> String {
    let base: String = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let base = base.trim_matches('-');
    let mut name = if base.is_empty() {
        harness.to_string()
    } else {
        format!("{harness}-{base}")
    };
    name.truncate(MAX_NAME);
    name.trim_end_matches('-').to_string()
}

/// `~` and `~/...` against `$HOME`; anything else as given.
pub fn expand_home(path: &str, home: Option<OsString>) -> PathBuf {
    match (path.strip_prefix('~'), home) {
        (Some(""), Some(h)) => PathBuf::from(h),
        (Some(rest), Some(h)) if rest.starts_with('/') => {
            PathBuf::from(h).join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    }
}

/// The folder the agent runs in, resolved as its roster row's `cwd` will show it.
#[cfg(unix)]
fn resolve_cwd(cwd: &str) -> Result<PathBuf> {
    let dir = expand_home(cwd, std::env::var_os("HOME"));
    let dir = std::fs::canonicalize(&dir)
        .with_context(|| format!("{} isn't a folder the agent can run in", dir.display()))?;
    ensure!(dir.is_dir(), "{} isn't a folder", dir.display());
    Ok(dir)
}

/// This machine's name, as the shim reports it in the roster row's `host.name`.
pub fn this_host() -> String {
    gethostname::gethostname()
        .into_string()
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Whether `program` names a file to run: a path, or a name on `PATH`.
pub fn on_path(program: &str, path: Option<OsString>) -> bool {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return p.is_file();
    }
    path.is_some_and(|dirs| std::env::split_paths(&dirs).any(|d| d.join(program).is_file()))
}

#[cfg(unix)]
impl LaunchBackend for Tmux {
    fn name(&self) -> &str {
        crate::address::LOCAL_BACKEND
    }

    fn check(&self, req: &LaunchRequest) -> Result<()> {
        ensure!(
            !req.argv.is_empty(),
            "the launch request has no command to run"
        );
        ensure!(
            req.options.is_empty(),
            "the tmux adapter takes no options, but was given {:?}",
            req.options
        );
        ensure!(
            valid_name(&req.name),
            "{:?} can't name a tmux session: use up to {MAX_NAME} letters, digits, - or _",
            req.name
        );
        resolve_cwd(&req.cwd)?;
        // The PATH the agent will run with: the client's, if a test set one.
        let path = self
            .client_env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| OsString::from(v))
            .or_else(|| std::env::var_os("PATH"));
        ensure!(
            on_path(&self.program, path.clone()),
            "{} isn't installed (not found on PATH), so nothing was started",
            self.program
        );
        ensure!(
            on_path(&req.argv[0], path),
            "{} isn't on PATH here, so nothing was started",
            req.argv[0]
        );
        let mut shim = Command::new(&self.shim[0]);
        shim.args(&self.shim[1..]).arg("--version");
        let out = run_bounded(shim, Vec::new(), Duration::from_secs(5))
            .with_context(|| format!("running `{} --version`", self.shim[0]))?;
        crate::check_shim_version(&String::from_utf8_lossy(&out.stdout), &req.marshal_version)?;
        if let Some(addr) = req.env.get("MARSHAL_DAEMON_ADDRESS") {
            // The agent's config dirs: the XDG_CONFIG_HOME it's given, else ours.
            let xdg = req
                .env
                .get("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from));
            let dirs =
                crate::address::config_dirs_from(xdg, std::env::var_os("HOME").map(PathBuf::from));
            crate::address::check_daemon_address_file(&dirs, addr)?;
        }
        Ok(())
    }

    fn recovery_handle(&self, req: &LaunchRequest) -> Result<String> {
        session_key(&req.agent_id)
    }

    fn launch(&self, req: &LaunchRequest) -> Result<Launched> {
        ensure!(
            !req.argv.is_empty(),
            "the launch request has no command to run"
        );
        let handle = self.recovery_handle(req)?;
        if let Some(launched) = self.recover(req)? {
            return Ok(launched);
        }
        let cwd = resolve_cwd(&req.cwd)?;
        let mut launch_env = req.env.clone();
        launch_env.remove("MARSHAL_LAUNCH_KEY");
        for identity in ["MARSHAL_AGENT_ID", "MARSHAL_LAUNCH_ID"] {
            launch_env.entry(identity.to_owned()).or_default();
        }
        launch_env.insert("MARSHAL_LAUNCH_CWD".to_owned(), cwd.display().to_string());
        let mut pane_env = agent_tmux::PaneEnvironment::default();
        for (key, value) in launch_env {
            if !DAEMON_CREDENTIAL_ENV.contains(&key.as_str()) {
                pane_env = pane_env.set(key, value);
            }
        }
        for key in DAEMON_CREDENTIAL_ENV {
            pane_env = pane_env.remove(key);
        }
        let mut create = agent_tmux::CreateSession::new(
            agent_tmux::SessionName::new(&handle)?,
            agent_tmux::LiteralCommand::new(
                &req.argv[0],
                req.argv[1..].iter().map(OsString::from),
                cwd.clone(),
            )?,
        );
        create.pane_env = pane_env;
        create.marker = Some(agent_tmux::OwnershipMarker::new(
            "MARSHAL_LAUNCH_KEY",
            &handle,
        )?);
        self.with_server(
            |server, runtime| match runtime.block_on(server.create(create)) {
                Ok(session) => Ok(self.observation(
                    req,
                    handle,
                    cwd.display().to_string(),
                    server.attach_command(&session),
                )),
                Err(error) if error.kind() == agent_tmux::ErrorKind::DuplicateName => self
                    .recover(req)?
                    .context("tmux reported a duplicate launch but it could not be recovered"),
                Err(error) => Err(error.into()),
            },
        )
    }

    fn recover(&self, req: &LaunchRequest) -> Result<Option<Launched>> {
        let handle = self.recovery_handle(req)?;
        self.with_server(|server, runtime| {
            runtime.block_on(async {
                let Some(session) = owned_session(server, &handle).await? else {
                    return Ok(None);
                };
                let agent_tmux::EnvEntry::Value(cwd) = server
                    .session_env(session.id, OsStr::new("MARSHAL_LAUNCH_CWD"))
                    .await?
                else {
                    bail!("tmux launch directory was not recorded");
                };
                let cwd = cwd.to_str().context("invalid tmux launch directory")?;
                Ok(Some(self.observation(
                    req,
                    handle,
                    cwd.to_owned(),
                    server.attach_command(&session),
                )))
            })
        })
    }

    fn stop(&self, handle: &str) -> Result<StopReceipt> {
        let target = owned_target(handle)?;
        self.with_server(
            |server, runtime| match runtime.block_on(server.stop_owned(&target))? {
                agent_tmux::StopOutcome::Stopped => Ok(StopReceipt::Stopped),
                agent_tmux::StopOutcome::Gone => Ok(StopReceipt::Gone),
                agent_tmux::StopOutcome::Mismatch => {
                    bail!("tmux session {handle} does not identify this launch")
                }
            },
        )
    }

    fn status(&self, handle: &str) -> Result<Option<bool>> {
        self.with_server(|server, runtime| {
            Ok(Some(
                runtime.block_on(owned_session(server, handle))?.is_some(),
            ))
        })
    }
}

#[cfg(not(unix))]
impl LaunchBackend for Tmux {
    fn name(&self) -> &str {
        crate::address::LOCAL_BACKEND
    }
    fn check(&self, _: &LaunchRequest) -> Result<()> {
        bail!(
            "native tmux ({}) requires Unix; use the ssh backend for a Unix host",
            self.program
        )
    }
    fn recovery_handle(&self, req: &LaunchRequest) -> Result<String> {
        session_key(&req.agent_id)
    }
    fn launch(&self, _: &LaunchRequest) -> Result<Launched> {
        bail!("native tmux requires Unix")
    }
    fn recover(&self, _: &LaunchRequest) -> Result<Option<Launched>> {
        bail!("native tmux requires Unix")
    }
    fn stop(&self, _: &str) -> Result<StopReceipt> {
        bail!("native tmux requires Unix")
    }
    fn status(&self, _: &str) -> Result<Option<bool>> {
        bail!("native tmux requires Unix")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn names_are_kept_to_what_a_tmux_target_can_take() {
        assert!(valid_name("claude-marshal_2"));
        for bad in ["", "a.b", "a:b", "a b"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(!valid_name(&"x".repeat(MAX_NAME + 1)));
        assert_eq!(
            default_name("claude", Path::new("/home/j/my repo.v2")),
            "claude-my-repo-v2"
        );
        assert_eq!(default_name("codex", Path::new("/")), "codex");
        assert!(valid_name(&default_name(
            "codex",
            Path::new(&format!("/{}", "y".repeat(200)))
        )));
    }

    #[test]
    fn home_is_expanded_only_at_the_front() {
        let home = Some(OsString::from("/home/u"));
        assert_eq!(expand_home("~", home.clone()), PathBuf::from("/home/u"));
        assert_eq!(
            expand_home("~/w/app", home.clone()),
            PathBuf::from("/home/u/w/app")
        );
        assert_eq!(expand_home("~bob/x", home.clone()), PathBuf::from("~bob/x"));
        assert_eq!(expand_home("/a/~", home), PathBuf::from("/a/~"));
        assert_eq!(expand_home("~/x", None), PathBuf::from("~/x"));
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_folder_or_a_bad_name_starts_nothing() {
        let t = Tmux::new("/nonexistent/tmux");
        let mut req = LaunchRequest {
            version: crate::PROTOCOL_VERSION,
            harness: "claude".into(),
            argv: v(&["claude"]),
            cwd: "/nonexistent/folder".into(),
            name: "ok".into(),
            agent_id: "launch-x".into(),
            marshal_version: crate::MARSHAL_VERSION.into(),
            env: Default::default(),
            options: vec![],
        };
        assert!(
            t.check(&req)
                .unwrap_err()
                .to_string()
                .contains("isn't a folder")
        );
        req.cwd = "/".into();
        assert!(
            t.check(&req)
                .unwrap_err()
                .to_string()
                .contains("isn't installed")
        );
        req.name = "a:b".into();
        assert!(t.check(&req).is_err());
        assert!(t.launch(&req).is_err());
        req.name = "ok".into();
        req.options = v(&["--host", "x"]);
        assert!(
            t.check(&req)
                .unwrap_err()
                .to_string()
                .contains("no options")
        );
    }
}
