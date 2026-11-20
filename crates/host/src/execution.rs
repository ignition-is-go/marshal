use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use marshal_client::host::{ExecutionRequest, ExecutionResult, Harness};
use tokio::process::Command;

pub struct Execution {
    pub harnesses: Vec<Harness>,
    pub workspace_root: Option<PathBuf>,
}

impl Execution {
    pub async fn execute(&self, request: ExecutionRequest, host_token: &str) -> ExecutionResult {
        match self.run(request, host_token).await {
            Ok(result) => result,
            Err(error) => ExecutionResult::Rejected {
                reason: error.to_string(),
            },
        }
    }

    async fn run(&self, request: ExecutionRequest, host_token: &str) -> Result<ExecutionResult> {
        let launch_id = match &request {
            ExecutionRequest::Start { launch_id, .. }
            | ExecutionRequest::Inspect { launch_id }
            | ExecutionRequest::Stop { launch_id } => {
                uuid::Uuid::parse_str(launch_id).context("invalid launch ID")?
            }
        };
        let session = format!("marshal-{launch_id}");
        let target = format!("={session}");
        match request {
            ExecutionRequest::Start {
                harness, workspace, ..
            } => {
                anyhow::ensure!(
                    self.harnesses.contains(&harness),
                    "host has not opted into this harness"
                );
                let root = self
                    .workspace_root
                    .as_ref()
                    .context("host execution is disabled")?;
                let workspace =
                    std::fs::canonicalize(workspace).context("resolving launch workspace")?;
                anyhow::ensure!(
                    workspace.starts_with(root) && workspace.is_dir(),
                    "workspace is outside the configured root"
                );
                let (harness_name, launcher) = match harness {
                    Harness::Claude => ("claude", &["claude"][..]),
                    Harness::Codex => ("codex", &["marshal-stdio", "codex-run"][..]),
                };
                let cwd = workspace.to_str().context("workspace path is not UTF-8")?;
                let intent = format!("{harness_name}:{cwd}");
                // tmux expands formats in -c. Preserve literal path components.
                let tmux_cwd = cwd.replace('#', "##");
                if exists(&target).await? {
                    verify_intent(&target, &intent).await?;
                } else {
                    // Keep the directory before calling tmux: a timeout can
                    // leave a running process that still has to report its join.
                    let mut state = tempfile::Builder::new();
                    state.prefix("marshal-join-");
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt as _;
                        state.permissions(std::fs::Permissions::from_mode(0o700));
                    }
                    let join_dir = state.tempdir()?;
                    let proof_file = join_dir.path().join("assignment-proof");
                    std::fs::write(
                        &proof_file,
                        marshal_entities::assignment_proof(host_token, &launch_id.to_string()),
                    )
                    .context("writing launch assignment proof")?;
                    let proof_file = proof_file.to_str().context("proof path is not UTF-8")?;
                    let join_file = join_dir.path().join("session");
                    let join_file = join_file.to_str().context("join path is not UTF-8")?;
                    let intent_env = format!("MARSHAL_LAUNCH_INTENT={intent}");
                    let join_env = format!("MARSHAL_HOST_JOIN_FILE={join_file}");
                    let launch_env = format!("MARSHAL_AGENT_ID={launch_id}");
                    let proof_env = format!("MARSHAL_ASSIGNMENT_PROOF_FILE={proof_file}");
                    let join_dir = join_dir.keep();
                    let mut args = vec![
                        "new-session",
                        "-d",
                        "-s",
                        &session,
                        "-c",
                        &tmux_cwd,
                        "-e",
                        &intent_env,
                        "-e",
                        &join_env,
                        "env",
                        &launch_env,
                        &proof_env,
                    ];
                    args.extend_from_slice(launcher);
                    match tmux(&args).await {
                        Ok(output) if output.status.success() => {}
                        Ok(_) => {
                            // The stable name may already exist after a duplicate start.
                            let _ = std::fs::remove_dir(&join_dir);
                            if !matches!(verify_intent(&target, &intent).await, Ok(())) {
                                return Ok(ExecutionResult::Unknown);
                            }
                        }
                        // The host cannot prove whether tmux acted before losing the reply.
                        Err(_) => return Ok(ExecutionResult::Unknown),
                    }
                    if !matches!(exists(&target).await, Ok(true)) {
                        return Ok(ExecutionResult::Unknown);
                    }
                }
                Ok(ExecutionResult::Running { handle: session })
            }
            ExecutionRequest::Inspect { .. } => {
                if exists(&target).await? {
                    inspect_join(&target, session).await
                } else {
                    Ok(ExecutionResult::Unknown)
                }
            }
            ExecutionRequest::Stop { .. } => {
                if exists(&target).await? {
                    let output = tmux(&["kill-session", "-t", &target]).await?;
                    anyhow::ensure!(
                        output.status.success() || !exists(&target).await?,
                        "could not stop agent"
                    );
                }
                Ok(ExecutionResult::Stopped)
            }
        }
    }
}

pub fn workspace_root(path: &Path) -> Result<PathBuf> {
    let root = path
        .canonicalize()
        .context("resolving MARSHAL_HOST_WORKSPACE_ROOT")?;
    anyhow::ensure!(
        root.is_dir(),
        "MARSHAL_HOST_WORKSPACE_ROOT is not a directory"
    );
    Ok(root)
}

async fn exists(target: &str) -> Result<bool> {
    let output = tmux(&["has-session", "-t", target]).await?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::ensure!(
        stderr.contains("can't find session")
            || stderr.contains("no server running")
            || stderr.contains("No such file or directory"),
        "could not inspect tmux session"
    );
    Ok(false)
}

async fn inspect_join(target: &str, handle: String) -> Result<ExecutionResult> {
    let running = || ExecutionResult::Running {
        handle: handle.clone(),
    };

    let output = tmux(&["show-environment", "-t", target, "MARSHAL_HOST_JOIN_FILE"]).await?;
    if !output.status.success() {
        return Ok(running());
    }
    let Some(path) = std::str::from_utf8(&output.stdout)?
        .strip_prefix("MARSHAL_HOST_JOIN_FILE=")
        .and_then(|line| line.strip_suffix('\n'))
    else {
        return Ok(running());
    };
    let path = Path::new(path);
    let Some(session_id) = read_marker(path)? else {
        return Ok(running());
    };
    if let Some(task_id) = read_marker(&path.with_file_name("accepted-task"))? {
        return Ok(ExecutionResult::Accepted {
            handle,
            session_id,
            task_id,
        });
    }
    Ok(ExecutionResult::Attached { handle, session_id })
}

fn read_marker(path: &Path) -> Result<Option<String>> {
    use std::io::Read as _;

    let mut file = match std::fs::File::open(path) {
        Ok(file) => file.take(1024),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    // A hook can still be writing the record, so a truncated record just
    // means the session is still joining.
    Ok(serde_json::from_slice::<String>(&bytes)
        .ok()
        .filter(|id| !id.is_empty()))
}

async fn verify_intent(target: &str, expected: &str) -> Result<()> {
    let output = tmux(&["show-environment", "-t", target, "MARSHAL_LAUNCH_INTENT"]).await?;
    let value = std::str::from_utf8(&output.stdout)?
        .strip_suffix('\n')
        .unwrap_or_default();
    anyhow::ensure!(
        output.status.success() && value.strip_prefix("MARSHAL_LAUNCH_INTENT=") == Some(expected),
        "launch ID unavailable or already used for a different request"
    );
    Ok(())
}

async fn tmux(args: &[&str]) -> Result<std::process::Output> {
    let mut command = Command::new("tmux");
    command
        .args(["-L", "marshal-host"])
        .args(args)
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(4), command.output())
        .await
        .context("tmux request timed out")?
        .context("running tmux")
}
