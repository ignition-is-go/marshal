mod execution;

use execution::Execution;
use std::{collections::BTreeMap, path::Path, time::Duration};

use anyhow::{Context, Result};
use marshal_client::host::{HOST_HEARTBEAT_INTERVAL, Harness, HostConnection};

const USAGE: &str = "Usage: marshal-host [--profile <path>] [--launch-harness claude|codex]...

MARSHAL_HOST_URL         Host WebSocket URL ending in /host
MARSHAL_HOST_ID          Host ID configured on marshal-server
MARSHAL_HOST_TOKEN_FILE  Path to this host's bearer token
MARSHAL_HOST_WORKSPACE_ROOT  Required directory containing launch workspaces

With no --launch-harness flags, the host advertises no ability to launch agents.";

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut launch_harnesses = Vec::new();
    let mut profile = BTreeMap::<String, String>::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--profile" => {
                let path = args.next().context("--profile requires a path")?;
                profile =
                    serde_json::from_slice(&std::fs::read(path).context("reading host profile")?)
                        .context("parsing host profile")?;
            }
            "--launch-harness" => {
                let harness = match args.next().as_deref() {
                    Some("claude") => Harness::Claude,
                    Some("codex") => Harness::Codex,
                    _ => anyhow::bail!("--launch-harness requires claude or codex"),
                };
                if !launch_harnesses.contains(&harness) {
                    launch_harnesses.push(harness);
                }
            }
            _ => anyhow::bail!("unknown argument {arg}; see --help"),
        }
    }
    let settings = |name: &str| {
        profile
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok())
            .with_context(|| format!("{name} is required"))
    };
    let url = settings("MARSHAL_HOST_URL")?;
    let host_id = settings("MARSHAL_HOST_ID")?;
    let token_file = settings("MARSHAL_HOST_TOKEN_FILE")?;
    let token = std::fs::read_to_string(token_file).context("reading host token file")?;

    anyhow::ensure!(!token.trim().is_empty(), "host token file is empty");
    let workspace_root = if launch_harnesses.is_empty() {
        None
    } else {
        let path = settings("MARSHAL_HOST_WORKSPACE_ROOT")?;
        Some(execution::workspace_root(Path::new(&path))?)
    };
    let execution = Execution {
        harnesses: launch_harnesses,
        workspace_root,
    };
    env_logger::init();
    tokio::select! {
        () = maintain_connection(&url, &host_id, token.trim(), &execution) => {}
        signal = tokio::signal::ctrl_c() => signal?,
    }
    Ok(())
}

async fn maintain_connection(url: &str, host_id: &str, token: &str, execution: &Execution) {
    loop {
        let result = run_connection(url, host_id, token, execution).await;
        if let Err(error) = result {
            log::warn!("host connection unavailable: {error}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn run_connection(
    url: &str,
    host_id: &str,
    token: &str,
    execution: &Execution,
) -> Result<()> {
    let mut connection = tokio::time::timeout(
        HOST_HEARTBEAT_INTERVAL,
        HostConnection::connect(url, host_id, token, execution.harnesses.clone()),
    )
    .await??;
    log::info!("registered host {}", connection.host_id);
    connection
        .run(|request| execution.execute(request, token))
        .await
}
