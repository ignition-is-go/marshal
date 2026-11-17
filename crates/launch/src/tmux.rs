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
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::{Adapter, LaunchRequest, Launched};

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

    fn tmux(&self) -> Command {
        let mut c = Command::new(&self.program);
        if let Some(s) = &self.socket {
            c.arg("-S").arg(s);
        }
        c.envs(self.client_env.iter().map(|(k, v)| (k, v)));
        c
    }

    /// `=name` matches that session exactly, not as a prefix of a longer name.
    fn has_session(&self, name: &str) -> Result<bool> {
        let status = self
            .tmux()
            .args(["has-session", "-t", &format!("={name}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("running {}: is tmux installed?", self.program))?;
        Ok(status.success())
    }
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
fn resolve_cwd(cwd: &str) -> Result<PathBuf> {
    let dir = expand_home(cwd, std::env::var_os("HOME"));
    let dir = std::fs::canonicalize(&dir)
        .with_context(|| format!("{} isn't a folder the agent can run in", dir.display()))?;
    ensure!(dir.is_dir(), "{} isn't a folder", dir.display());
    Ok(dir)
}

/// The `new-session` arguments. The command always goes as two or more
/// arguments, because tmux hands a lone one to `sh -c`: a one-word `argv`
/// goes through `env --`, which runs it as given and keeps the shell out.
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
    for (k, v) in env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    args.push("--".into());
    if argv.len() == 1 {
        args.extend(["env".to_string(), "--".to_string()]);
    }
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

impl Adapter for Tmux {
    fn name(&self) -> &str {
        crate::address::LOCAL_ADAPTER
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
        let out = Command::new(&self.shim[0])
            .args(&self.shim[1..])
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .with_context(|| {
                format!(
                    "running `{} --version`: is marshal-shim installed?",
                    self.shim[0]
                )
            })?;
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
        ensure!(
            !self.has_session(&req.name)?,
            "a tmux session named {0} already exists: attach with `tmux attach -t '={0}'`, or pick another --name",
            req.name
        );
        Ok(())
    }

    fn launch(&self, req: &LaunchRequest) -> Result<Launched> {
        ensure!(
            !req.argv.is_empty(),
            "the launch request has no command to run"
        );
        ensure!(
            valid_name(&req.name),
            "{:?} can't name a tmux session",
            req.name
        );
        let cwd = resolve_cwd(&req.cwd)?;
        let env: Vec<(String, String)> = req
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let out = self
            .tmux()
            .args(new_session_args(&req.name, &cwd, &env, &req.argv))
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}: is tmux installed?", self.program))?;
        if !out.status.success() {
            bail!(
                "tmux couldn't start {}: {}",
                req.argv.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        // The name the agent's shim reports: MARSHAL_HOST if it's given one.
        let host = req
            .env
            .get("MARSHAL_HOST")
            .filter(|h| !h.trim().is_empty())
            .cloned()
            .unwrap_or_else(this_host);
        Ok(Launched {
            handle: req.name.clone(),
            r#where: format!("{host} tmux:{}", req.name),
            host,
            cwd: cwd.display().to_string(),
            attach: Some(match &self.socket {
                // Quoted: a zsh shell expands an unquoted `=word` (EQUALS).
                Some(s) => format!(
                    "tmux -S '{}' attach -t '={}'",
                    s.display().to_string().replace('\'', r#"'"'"'"#),
                    req.name
                ),
                None => format!("tmux attach -t '={}'", req.name),
            }),
        })
    }

    fn stop(&self, handle: &str) -> Result<()> {
        ensure!(
            valid_name(handle),
            "{handle:?} isn't a tmux session this adapter made"
        );
        if !self.has_session(handle)? {
            return Ok(());
        }
        let out = self
            .tmux()
            .args(["kill-session", "-t", &format!("={handle}")])
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", self.program))?;
        ensure!(
            out.status.success() || !self.has_session(handle)?,
            "tmux couldn't stop session {handle}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(())
    }

    fn status(&self, handle: &str) -> Result<Option<bool>> {
        ensure!(
            valid_name(handle),
            "{handle:?} isn't a tmux session this adapter made"
        );
        self.has_session(handle).map(Some)
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

    #[test]
    fn a_missing_folder_or_a_bad_name_starts_nothing() {
        let t = Tmux::new("/nonexistent/tmux");
        let mut req = LaunchRequest {
            version: 1,
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
