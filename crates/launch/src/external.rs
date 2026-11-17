//! External adapters: an executable named `marshal-launch-<name>` on `PATH`.
//!
//! The launcher runs `marshal-launch-<name> <verb>`, writes one JSON request
//! on its stdin and reads one JSON object from its stdout. The adapter's
//! stderr is passed through to the user as it arrives, and is also kept, so a
//! failure carries the adapter's own reason. Exit 0 means the verb worked;
//! any other exit means it didn't, and for `launch` that nothing was started.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{Adapter, LaunchRequest, Launched, PROTOCOL_VERSION};

/// The prefix every external adapter's executable name starts with.
pub const ADAPTER_PREFIX: &str = "marshal-launch-";

/// The exit an adapter gives for a verb it doesn't implement. `status` is
/// optional: an adapter without it exits 2, and [`Adapter::status`] is then
/// `Ok(None)`, "can't tell". For any other verb exit 2 is a failure like any
/// other.
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
pub struct ExternalAdapter {
    name: String,
    program: PathBuf,
}

impl ExternalAdapter {
    /// Find `marshal-launch-<name>` on `PATH`.
    pub fn find(name: &str) -> Result<ExternalAdapter> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        Self::find_in(name, &path)
    }

    /// Find `marshal-launch-<name>` in the directories of a `PATH`-style list.
    pub fn find_in(name: &str, path: &OsStr) -> Result<ExternalAdapter> {
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            bail!("`{name}` is not an adapter name");
        }
        let exe = format!("{ADAPTER_PREFIX}{name}{}", std::env::consts::EXE_SUFFIX);
        std::env::split_paths(path)
            .map(|dir| dir.join(&exe))
            .find(|p| is_executable(p))
            .map(|program| ExternalAdapter {
                name: name.to_string(),
                program,
            })
            .ok_or_else(|| anyhow!("no `{name}` adapter: there is no executable {exe} on PATH"))
    }

    /// The executable this adapter runs.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Run one verb with `request` on stdin; return its stdout as `T`.
    fn call<T: DeserializeOwned>(&self, verb: &str, request: &Value) -> Result<T> {
        let shown = self.program.display();
        let mut child = Command::new(&self.program)
            .arg(verb)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {shown}"))?;

        // Pass stderr through as it arrives, and keep it for the error.
        let stderr = child.stderr.take().expect("stderr is piped");
        let tee = std::thread::spawn(move || {
            let mut kept = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("{line}");
                kept.push_str(&line);
                kept.push('\n');
            }
            kept
        });

        // An adapter that exits without reading its stdin is not an error
        // here; its exit status says what happened.
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let _ = serde_json::to_writer(&mut stdin, request)
            .and_then(|_| stdin.write_all(b"\n").map_err(serde_json::Error::io));
        drop(stdin);

        let out = child
            .wait_with_output()
            .with_context(|| format!("waiting for {shown}"))?;
        let err = tee.join().unwrap_or_default();
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
        if let Some(ver) = v.get("version")
            && ver != &json!(PROTOCOL_VERSION)
        {
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
struct Status {
    running: Option<bool>,
}

#[derive(serde::Deserialize)]
struct Empty {}

impl Adapter for ExternalAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(&self, req: &LaunchRequest) -> Result<()> {
        self.call::<Empty>("check", &serde_json::to_value(req)?)
            .map(|_| ())
    }

    fn launch(&self, req: &LaunchRequest) -> Result<Launched> {
        self.call("launch", &serde_json::to_value(req)?)
    }

    fn stop(&self, handle: &str) -> Result<()> {
        self.call::<Empty>(
            "stop",
            &json!({ "version": PROTOCOL_VERSION, "handle": handle }),
        )
        .map(|_| ())
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
