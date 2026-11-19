//! The built-in `tmux` adapter: a detached tmux session on this machine.
//!
//! tmux runs the request's `argv` with no shell in between and with the
//! request's `env` added through `new-session -e`, because a running tmux
//! server would otherwise give the session the server's environment, not the
//! launcher's.
//! None of the launcher's variables are secrets, which matters because `-e`
//! puts them on tmux's command line.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::{process::Command, time::Duration};

#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail, ensure};

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
        operation: impl FnOnce(&libtmux::Server, &libtmux::blocking::Runtime) -> Result<T> + Send,
    ) -> Result<T> {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = libtmux::blocking::Runtime::new()?;
                    let mut builder = libtmux::Server::builder()
                        .tmux_executable(&self.program)
                        .default_timeout(Duration::from_secs(5))
                        .output_limits(
                            libtmux::OutputLimits::default()
                                .max_stdout_bytes(1024 * 1024)
                                .max_stderr_bytes(1024 * 1024),
                        );
                    if let Some(socket) = &self.socket {
                        builder = builder.socket_path(socket);
                    }
                    for (key, value) in &self.client_env {
                        builder = builder.env(key, value);
                    }
                    for key in DAEMON_CREDENTIAL_ENV {
                        builder = builder.env_remove(key);
                    }
                    let server = builder.env("LC_ALL", "C").build()?;
                    let result = operation(&server, &runtime);
                    let shutdown = runtime.run(server.shutdown());
                    let value = result?;
                    shutdown?;
                    Ok(value)
                })
                .join()
                .map_err(|_| anyhow::anyhow!("tmux operation thread panicked"))?
        })
    }

    #[cfg(unix)]
    fn observation(&self, req: &LaunchRequest, handle: String, cwd: String) -> Launched {
        let host = req
            .env
            .get("MARSHAL_HOST")
            .filter(|h| !h.trim().is_empty())
            .cloned()
            .unwrap_or_else(this_host);
        let attach = match &self.socket {
            Some(s) => format!(
                "tmux -S '{}' attach -t '={handle}'",
                s.display().to_string().replace('\'', r#"'"'"'"#),
            ),
            None => format!("tmux attach -t '={handle}'"),
        };
        Launched {
            handle,
            r#where: format!("{host} tmux:{}", req.name),
            host,
            cwd,
            attach: Some(attach),
        }
    }

    #[cfg(unix)]
    fn session_id(&self, handle: &str) -> Result<Option<String>> {
        self.with_server(|server, runtime| {
            Ok(runtime
                .run(owned_session(server, handle))?
                .map(|session| session.id().to_string()))
        })
    }
}

#[cfg(unix)]
async fn owned_session(server: &libtmux::Server, handle: &str) -> Result<Option<libtmux::Session>> {
    ensure!(
        handle.starts_with("marshal-") && valid_name(handle),
        "invalid tmux recovery handle"
    );
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    let session = loop {
        match server.session(handle).await {
            Ok(session) => break session,
            Err(libtmux::Error::ServerGone {
                kind: libtmux::ServerGoneKind::NotRunning,
                ..
            }) => return Ok(None),
            Err(
                error @ libtmux::Error::ServerGone {
                    kind: libtmux::ServerGoneKind::Unreachable,
                    ..
                },
            ) => {
                if !server.socket_path().try_exists()? {
                    return Ok(None);
                }
                if std::time::Instant::now() >= deadline {
                    return Err(error.into());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let Some(session) = session else {
        return Ok(None);
    };
    let marker = session.environment("MARSHAL_LAUNCH_KEY").await?;
    ensure!(
        matches!(marker, Some(libtmux::EnvironmentEntry::Set(value)) if value.as_bytes() == handle.as_bytes()),
        "tmux session {handle} does not identify this launch"
    );
    Ok(Some(session))
}

#[cfg(unix)]
fn tmux_command(args: &[String]) -> libtmux::Command {
    args[1..]
        .iter()
        .fold(libtmux::Command::new(&args[0]), |command, arg| {
            command.arg(arg)
        })
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

/// Build a detached session command. Strip daemon credentials inside the
/// pane too, because an existing tmux server can already hold them.
pub fn new_session_args(
    name: &str,
    cwd: &Path,
    env: &[(String, String)],
    argv: &[String],
) -> Vec<String> {
    let mut args = vec![
        "new-session".to_string(),
        "-d".into(),
        "-s".into(),
        name.into(),
        "-c".into(),
        cwd.display().to_string(),
    ];
    for (key, value) in env
        .iter()
        .filter(|(key, _)| !DAEMON_CREDENTIAL_ENV.contains(&key.as_str()))
    {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    args.extend(["--".to_string(), "env".to_string()]);
    for key in DAEMON_CREDENTIAL_ENV {
        args.extend(["-u".to_string(), (*key).to_string()]);
    }
    args.push("--".into());
    args.extend(argv.iter().cloned());
    args
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
        for identity in ["MARSHAL_AGENT_ID", "MARSHAL_LAUNCH_ID"] {
            launch_env.entry(identity.to_owned()).or_default();
        }
        launch_env.insert("MARSHAL_LAUNCH_KEY".to_owned(), handle.clone());
        launch_env.insert("MARSHAL_LAUNCH_CWD".to_owned(), cwd.display().to_string());
        let env: Vec<_> = launch_env.into_iter().collect();
        let args = new_session_args(&handle, &cwd, &env, &req.argv);
        let collision = self.with_server(|server, runtime| {
            let out = runtime.run(server.cmd(tmux_command(&args)))?;
            if out.success() {
                return Ok(false);
            }
            if matches!(
                out.refusal_for("new-session"),
                Some(libtmux::Error::SessionExists { .. })
            ) {
                return Ok(true);
            }
            bail!(
                "tmux could not start the requested agent: {}",
                out.stderr_lossy().trim()
            );
        })?;
        if collision {
            return self
                .recover(req)?
                .context("tmux reported a duplicate launch but it could not be recovered");
        }
        Ok(self.observation(req, handle, cwd.display().to_string()))
    }

    fn recover(&self, req: &LaunchRequest) -> Result<Option<Launched>> {
        let handle = self.recovery_handle(req)?;
        self.with_server(|server, runtime| {
            runtime.run(async {
                let Some(session) = owned_session(server, &handle).await? else {
                    return Ok(None);
                };
                let Some(libtmux::EnvironmentEntry::Set(cwd)) =
                    session.environment("MARSHAL_LAUNCH_CWD").await?
                else {
                    bail!("tmux launch directory was not recorded");
                };
                let cwd =
                    std::str::from_utf8(cwd.as_bytes()).context("invalid tmux launch directory")?;
                Ok(Some(self.observation(req, handle, cwd.to_owned())))
            })
        })
    }

    fn stop(&self, handle: &str) -> Result<StopReceipt> {
        let Some(id) = self.session_id(handle)? else {
            return Ok(StopReceipt::Gone);
        };
        let owns_session = format!("#{{==:#{{MARSHAL_LAUNCH_KEY}},{handle}}}");
        let kill = format!(r#"kill-session -t "{id}" ; display-message -p marshal-stopped"#);
        let args = [
            "if-shell",
            "-F",
            "-t",
            &id,
            &owns_session,
            &kill,
            "display-message -p marshal-mismatch",
        ]
        .map(str::to_owned);
        let stopped = self.with_server(|server, runtime| {
            let out = runtime.run(server.cmd(tmux_command(&args)))?;
            Ok(out.success() && out.stdout() == b"marshal-stopped\n")
        })?;
        if stopped {
            return Ok(StopReceipt::Stopped);
        }
        ensure!(
            self.session_id(handle)?.is_none(),
            "tmux did not confirm stopping {handle}"
        );
        Ok(StopReceipt::Gone)
    }

    fn status(&self, handle: &str) -> Result<Option<bool>> {
        self.session_id(handle).map(|id| Some(id.is_some()))
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

    #[test]
    fn argv_runs_as_given_with_env_added_and_no_shell() {
        let env = v(&["MARSHAL_AGENT_ID", "launch-1"]);
        let env = [(env[0].clone(), env[1].clone())];
        let args = new_session_args("n", Path::new("/w"), &env, &v(&["claude"]));
        assert_eq!(
            args,
            v(&[
                "new-session",
                "-d",
                "-s",
                "n",
                "-c",
                "/w",
                "-e",
                "MARSHAL_AGENT_ID=launch-1",
                "--",
                "env",
                "-u",
                "MYKO_POSTGRES_URL",
                "-u",
                "MARSHAL_AUTH_POSTGRES_URL",
                "-u",
                "MARSHAL_AUTH_CLIENT_SECRET",
                "--",
                "claude"
            ])
        );
        let args = new_session_args(
            "n",
            Path::new("/w"),
            &[],
            &v(&["marshal-shim", "codex-run"]),
        );
        assert_eq!(
            args[args.len() - 3..],
            v(&["--", "marshal-shim", "codex-run"])[..]
        );
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
