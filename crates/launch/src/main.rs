//! `marshal-launch <backend> <harness>`: start an agent and wait for it to join.
//! See docs/agent-launcher.md.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use marshal_launch::{address, api, new_agent_id, tmux};

const USAGE: &str =
    "usage: marshal-launch <backend> <claude|codex> [--cwd <dir>] [--name <label>] \
[--task <text>] [--daemon <url>] [--timeout <secs>] [--no-check] [--id <launch-id>] [-- <backend options>]
       marshal-launch list [--daemon <url>]
       marshal-launch get|stop <launch-id>";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
struct Args {
    adapter: String,
    harness: String,
    cwd: Option<String>,
    name: Option<String>,
    daemon: Option<String>,
    launch_id: Option<String>,
    timeout: Duration,
    check: bool,
    options: Vec<String>,
    task: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut it = args.iter();
    let adapter = it.next().context(USAGE)?.clone();
    let harness = it.next().context(USAGE)?.clone();
    marshal_launch::harness_argv(&harness).context(USAGE)?;
    let mut a = Args {
        adapter,
        harness,
        cwd: None,
        name: None,
        daemon: None,
        launch_id: None,
        timeout: DEFAULT_TIMEOUT,
        check: true,
        options: vec![],
        task: None,
    };
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--cwd" => a.cwd = Some(value()?),
            "--name" => a.name = Some(value()?),
            "--daemon" => a.daemon = Some(value()?),
            "--id" => a.launch_id = Some(value()?),
            "--timeout" => {
                let secs: u64 = value()?
                    .parse()
                    .with_context(|| format!("--timeout takes whole seconds\n{USAGE}"))?;
                ensure!(secs > 0, "--timeout must be at least 1 second");
                a.timeout = Duration::from_secs(secs);
            }
            "--no-check" => a.check = false,
            "--task" => {
                let task = value()?;
                ensure!(!task.trim().is_empty(), "--task needs some text\n{USAGE}");
                a.task = Some(task);
            }
            // Everything after `--` is the adapter's, never the harness's.
            "--" => {
                a.options = it.by_ref().cloned().collect();
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(a)
}

fn service_base(daemon: Option<String>) -> Result<String> {
    if let Some(daemon) = daemon {
        if daemon.starts_with("http://") || daemon.starts_with("https://") {
            return Ok(daemon);
        }
        return api::service_url(&daemon);
    }
    match std::env::var("MARSHAL_LAUNCH_API") {
        Ok(base) => Ok(base),
        Err(_) => api::service_url(&address::from_environment(None)),
    }
}

fn run(args: Args) -> Result<()> {
    let base = service_base(args.daemon)?;
    let cwd = match args.cwd {
        Some(cwd) => cwd,
        None if args.adapter == address::LOCAL_BACKEND => {
            std::env::current_dir()?.display().to_string()
        }
        None => bail!("--cwd is needed for the {} backend", args.adapter),
    };
    let name = args
        .name
        .unwrap_or_else(|| tmux::default_name(&args.harness, &PathBuf::from(&cwd)));
    let launch_id = args.launch_id.unwrap_or_else(new_agent_id);
    eprintln!("launch {launch_id}");
    let response: api::LaunchView = runtime()?.block_on(api::call(
        &base,
        api::LAUNCH_PATH,
        &api::StartLaunch {
            launch_id,
            backend: args.adapter,
            intent: marshal_launch::LaunchIntent::Process {
                harness: args.harness,
                cwd,
                options: args.options,
            },
            name,
            check: args.check,
            timeout_secs: args.timeout.as_secs(),
            first_task: args.task,
        },
    ))?;
    report(&response)
}

fn report(view: &api::LaunchView) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(view)?);
    if let Some(error) = &view.error {
        bail!("{error}");
    }
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")
}

fn dispatch(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let daemon = match &args[1..] {
                [] => None,
                [flag, url] if flag == "--daemon" => Some(url.clone()),
                _ => bail!("list takes only --daemon <url>\n{USAGE}"),
            };
            let launches: Vec<api::LaunchView> = runtime()?.block_on(api::call(
                &service_base(daemon)?,
                api::LIST_LAUNCHES_PATH,
                &serde_json::json!({}),
            ))?;
            println!("{}", serde_json::to_string_pretty(&launches)?);
            Ok(())
        }
        Some(operation @ ("get" | "stop")) => {
            let (key, daemon) = match &args[1..] {
                [key] => (key, None),
                [key, flag, url] if flag == "--daemon" => (key, Some(url.clone())),
                _ => bail!("{operation} takes a launch ID and optional --daemon <url>\n{USAGE}"),
            };
            let path = if operation == "get" {
                api::GET_LAUNCH_PATH
            } else {
                api::STOP_LAUNCH_PATH
            };
            let view: api::LaunchView = runtime()?.block_on(api::call(
                &service_base(daemon)?,
                path,
                &api::LaunchKey {
                    launch_id: key.clone(),
                },
            ))?;
            report(&view)
        }
        _ => parse_args(args).and_then(run),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = dispatch(&args) {
        eprintln!("marshal-launch: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_an_adapter_a_harness_and_its_options() {
        let a = parse_args(&v(&[
            "ssh",
            "codex",
            "--cwd",
            "~/w",
            "--name",
            "dev",
            "--daemon",
            "ws://m:6155",
            "--timeout",
            "5",
            "--no-check",
            "--",
            "--host",
            "build-03",
        ]))
        .unwrap();
        assert_eq!(a.adapter, "ssh");
        assert_eq!(a.harness, "codex");
        assert_eq!(a.cwd.as_deref(), Some("~/w"));
        assert_eq!(a.name.as_deref(), Some("dev"));
        assert_eq!(a.daemon.as_deref(), Some("ws://m:6155"));
        assert_eq!(a.timeout, Duration::from_secs(5));
        assert!(!a.check);
        assert_eq!(a.options, ["--host", "build-03"]);
        let a = parse_args(&v(&["tmux", "claude", "--task", "say ready"])).unwrap();
        assert_eq!(a.task.as_deref(), Some("say ready"));
        assert!(parse_args(&v(&["tmux", "claude", "--task", " "])).is_err());
        let a = parse_args(&v(&["tmux", "claude"])).unwrap();
        assert_eq!((a.timeout, a.check), (DEFAULT_TIMEOUT, true));
    }

    #[test]
    fn refuses_anything_it_doesnt_know_including_harness_flags() {
        assert!(parse_args(&v(&[])).is_err());
        assert!(parse_args(&v(&["tmux"])).is_err());
        assert!(parse_args(&v(&["tmux", "bash"])).is_err());
        assert!(parse_args(&v(&["tmux", "claude", "--cwd"])).is_err());
        assert!(parse_args(&v(&["tmux", "claude", "--timeout", "0"])).is_err());
        assert!(parse_args(&v(&["tmux", "claude", "--dangerously-skip-permissions"])).is_err());
        assert!(dispatch(&v(&["list", "x"])).is_err());
        assert!(dispatch(&v(&["list", "--daemon"])).is_err());
        assert!(dispatch(&v(&["stop"])).is_err());
    }
}
