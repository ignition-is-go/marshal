//! External adapters: an executable named `marshal-launch-<name>` on `PATH`.
//!
//! The launcher runs `marshal-launch-<name> <verb>`, writes one JSON request
//! on its stdin and reads one JSON object from its stdout. The adapter's
//! stderr accompanies the adapter result. A failing launch can still have
//! started a target under its recovery handle.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;

use crate::{LaunchBackend, LaunchRequest, Launched, PROTOCOL_VERSION, StopReceipt};

/// The prefix every external adapter's executable name starts with.
pub const ADAPTER_PREFIX: &str = "marshal-launch-";

/// Marshal daemon credentials must not be inherited by launch subprocesses.
pub const DAEMON_CREDENTIAL_ENV: &[&str] = &[
    "MYKO_POSTGRES_URL",
    "MARSHAL_AUTH_POSTGRES_URL",
    "MARSHAL_AUTH_CLIENT_SECRET",
];

/// Exit status for an unsupported optional `status` or `inspect` operation.
pub const UNKNOWN_VERB_EXIT: i32 = 2;

/// An adapter exited [`UNKNOWN_VERB_EXIT`].
#[derive(Debug)]
struct UnknownVerb(String);

impl std::fmt::Display for UnknownVerb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UnknownVerb {}

/// An adapter found as `marshal-launch-<name>` on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalBackend {
    name: String,
    program: PathBuf,
}

impl ExternalBackend {
    /// Find `marshal-launch-<name>` on `PATH`.
    pub fn find(name: &str) -> Result<ExternalBackend> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        Self::find_in(name, &path)
    }

    /// Find `marshal-launch-<name>` in the directories of a `PATH`-style list.
    pub fn find_in(name: &str, path: &OsStr) -> Result<ExternalBackend> {
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            bail!("`{name}` is not an adapter name");
        }
        let exe = format!("{ADAPTER_PREFIX}{name}{}", std::env::consts::EXE_SUFFIX);
        std::env::split_paths(path)
            .map(|dir| dir.join(&exe))
            .find(|p| is_executable(p))
            .map(|program| ExternalBackend {
                name: name.to_string(),
                program,
            })
            .ok_or_else(|| anyhow!("no `{name}` adapter: there is no executable {exe} on PATH"))
    }

    /// The executable this adapter runs.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Run an adapter verb with its JSON request and read its JSON response.
    fn call<T: DeserializeOwned>(&self, verb: &str, request: &Value) -> Result<T> {
        let shown = self.program.display();
        let mut command = Command::new(&self.program);
        command.arg(verb);
        for key in DAEMON_CREDENTIAL_ENV {
            command.env_remove(key);
        }
        let timeout = match verb {
            "recover" => Duration::from_secs(5),
            "status" | "stop" | "inspect" => Duration::from_secs(20),
            _ => Duration::from_secs(45),
        };
        let out = run_bounded(command, serde_json::to_vec(request)?, timeout)
            .with_context(|| format!("running {shown} {verb}"))?;
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.is_empty() {
            eprint!("{err}");
        }
        let reason = || {
            let e = err.trim();
            if e.is_empty() {
                "it said nothing on stderr".to_string()
            } else {
                e.to_string()
            }
        };
        match out.status.code() {
            Some(0) => {}
            Some(UNKNOWN_VERB_EXIT) => {
                return Err(UnknownVerb(format!(
                    "{ADAPTER_PREFIX}{} {verb} failed (exit {UNKNOWN_VERB_EXIT}): {}",
                    self.name,
                    reason()
                ))
                .into());
            }
            Some(c) => bail!(
                "{ADAPTER_PREFIX}{} {verb} failed (exit {c}): {}",
                self.name,
                reason()
            ),
            None => bail!(
                "{ADAPTER_PREFIX}{} {verb} was killed before it finished: {}",
                self.name,
                reason()
            ),
        }
        let v: Value = serde_json::from_slice(&out.stdout).with_context(|| {
            format!(
                "{ADAPTER_PREFIX}{} {verb} exited 0 but its stdout is not one JSON object: {:?}",
                self.name,
                String::from_utf8_lossy(&out.stdout).trim()
            )
        })?;
        if !v.is_object() {
            bail!(
                "{ADAPTER_PREFIX}{} {verb} answered {v}, not a JSON object",
                self.name
            );
        }
        let ver = v
            .get("version")
            .context("adapter response has no protocol version")?;
        if ver != &json!(PROTOCOL_VERSION) {
            bail!(
                "{ADAPTER_PREFIX}{} {verb} answered in contract version {ver}; this launcher speaks {PROTOCOL_VERSION}",
                self.name
            );
        }
        serde_json::from_value(v).with_context(|| {
            format!(
                "{ADAPTER_PREFIX}{} {verb} answered without the contract's fields",
                self.name
            )
        })
    }
}

#[derive(serde::Deserialize)]
struct Recovery {
    handle: String,
}

#[derive(serde::Deserialize)]
struct Inspection {
    launched: Option<Launched>,
}

#[derive(serde::Deserialize)]
struct Stop {
    state: StopState,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum StopState {
    Stopped,
    Gone,
}

#[derive(serde::Deserialize)]
struct Status {
    running: Option<bool>,
}

#[derive(serde::Deserialize)]
struct Empty {}

impl LaunchBackend for ExternalBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(&self, req: &LaunchRequest) -> Result<()> {
        self.call::<Empty>("check", &serde_json::to_value(req)?)
            .map(|_| ())
    }

    fn recovery_handle(&self, req: &LaunchRequest) -> Result<String> {
        let response: Recovery = self.call("recover", &serde_json::to_value(req)?)?;
        anyhow::ensure!(
            !response.handle.is_empty(),
            "adapter returned an empty recovery handle"
        );
        Ok(response.handle)
    }

    fn launch(&self, req: &LaunchRequest) -> Result<Launched> {
        self.call("launch", &serde_json::to_value(req)?)
    }

    fn recover(&self, req: &LaunchRequest) -> Result<Option<Launched>> {
        match self.call::<Inspection>("inspect", &serde_json::to_value(req)?) {
            Ok(response) => Ok(response.launched),
            Err(error) if error.downcast_ref::<UnknownVerb>().is_some() => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn stop(&self, handle: &str) -> Result<StopReceipt> {
        let receipt = self.call::<Stop>(
            "stop",
            &json!({ "version": PROTOCOL_VERSION, "handle": handle }),
        )?;
        Ok(match receipt.state {
            StopState::Stopped => StopReceipt::Stopped,
            StopState::Gone => StopReceipt::Gone,
        })
    }

    fn status(&self, handle: &str) -> Result<Option<bool>> {
        match self.call::<Status>(
            "status",
            &json!({ "version": PROTOCOL_VERSION, "handle": handle }),
        ) {
            Ok(s) => Ok(s.running),
            Err(e) if e.downcast_ref::<UnknownVerb>().is_some() => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Run a subprocess without blocking indefinitely on stdin, output, or exit.
pub fn run_bounded(command: Command, input: Vec<u8>, timeout: Duration) -> Result<Output> {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let mut child = tokio::process::Command::from(command)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;
            let mut stdin = child.stdin.take().context("opening subprocess stdin")?;
            let (output, written) = tokio::time::timeout(timeout, async move {
                tokio::join!(child.wait_with_output(), async move {
                    let result = stdin.write_all(&input).await;
                    drop(stdin);
                    result
                })
            })
            .await
            .with_context(|| format!("subprocess exceeded {}s", timeout.as_secs()))?;
            let output = output?;
            if let Err(error) = written
                && error.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Err(error).context("writing to subprocess");
            }
            Ok(output)
        })
    })
    .join()
    .map_err(|_| anyhow!("subprocess worker panicked"))?
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_hung_adapter_child_does_not_hold_the_caller() {
        let mut command = Command::new("sleep");
        command.arg("10");
        let started = std::time::Instant::now();
        let error = run_bounded(command, Vec::new(), Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("subprocess exceeded"));
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
