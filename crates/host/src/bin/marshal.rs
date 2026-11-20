use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use hyphae::Gettable;
use marshal_client::{
    ConnectionStatus, MarshalClient,
    config::{ADDRESS_FILE, config_file_candidates, server_address},
};
use marshal_entities::{
    AcceptTaskAssignment, AddTaskDependency, CompleteTaskAssignment, CoordinationMatches,
    CreateTask, DispatchAttempt, DispatchHarness, FindCoordination, GetAllDispatchHosts,
    GetAllSessions, GetAllTaskBlockers, GetAllTaskClaims, GetAllTasks, GetTaskById,
    ReleaseTaskClaim, RemoveTaskDependency, RenewTaskClaim, ReserveTaskDispatch, SendMessage,
    SendMessageResult, SessionId, SetTaskStatus, Task, TaskBlockerId, TaskClaim, TaskId,
    TaskPriority, TaskStatus,
};
use std::{sync::Arc, time::Duration};

const HELP: &str = "Usage: marshal <command>

  configure <server-url> [host-url]   Save Marshal server endpoints
  service install [host flags]       Install and start marshal-host as a user service
  service status                     Show marshal-host service status
  agents                             List sessions as JSON
  hosts                              List connected dispatch hosts as JSON (operator)
  find [--project <name>] [query]    Find task owners and idle agents as JSON
  task list [--project <name>]      List tasks as JSON (operator)
  task show <id>                    Show a task as JSON (operator)
  task create [--project <name>] [--body <text>] [--priority p0|p1|p2|p3]
              [--label <name>]... <title>
                                     Create a task, defaulting to Git origin
  task dep add|remove <id> <blocker-id>
                                     Change a task dependency
  task dep list <id>                 List dependencies as JSON
  task close <id> [commit]           Close a task with optional commit evidence
  task reopen <id>                   Reopen a task
  task accept <id>                   Accept the current launch's task assignment
  task claims                        List active claims as JSON (operator)
  task renew <id> <lease-id>         Extend an active lease (operator)
  task release <id> <lease-id>       Release a task lease (operator)
  task dispatch <id> <host> <claude|codex> <workspace>
                                     Reserve an agent launch for a task
  send --as <session-id> <to>... -- <message>
                                     Message one or more agents

The service reads MARSHAL_HOST_ID and MARSHAL_HOST_TOKEN_FILE at install time.
Set MARSHAL_HOST_URL if the host endpoint was not saved by configure.
Pass --launch-harness codex or --launch-harness claude to enable launches.";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => println!("{HELP}"),
        [flag] if matches!(flag.as_str(), "-h" | "--help") => println!("{HELP}"),
        [command, server, tail @ ..] if command == "configure" && tail.len() <= 1 => {
            validate_url(server)?;
            if let Some(host) = tail.first() {
                validate_url(host)?;
                write_config("host-address", host)?;
            }
            write_config(ADDRESS_FILE, server)?;
        }
        [command, action, flags @ ..] if command == "service" && action == "install" => {
            install_service(flags)?;
        }
        [command, action] if command == "service" && action == "status" => {
            service_status()?;
        }
        [command] if command == "hosts" => list_hosts().await?,
        [command] if command == "agents" => list_agents().await?,
        [command, flag, project, query @ ..] if command == "find" && flag == "--project" => {
            find_coordination(Some(project.clone()), query).await?;
        }
        [command, query @ ..]
            if command == "find" && !query.is_empty() && query[0] != "--project" =>
        {
            find_coordination(None, query).await?;
        }
        [command, flag, session, rest @ ..] if command == "send" && flag == "--as" => {
            let separator = rest
                .iter()
                .position(|arg| arg == "--")
                .context("separate recipients and message with --")?;
            let (recipients, body) = rest.split_at(separator);
            let body = body[1..].join(" ");
            anyhow::ensure!(
                !recipients.is_empty() && !body.trim().is_empty(),
                "send requires recipients and a message"
            );
            send_messages(session, recipients, &body).await?;
        }
        [command, action] if command == "task" && action == "list" => {
            list_tasks(None).await?;
        }
        [command, action, flag, project]
            if command == "task" && action == "list" && flag == "--project" =>
        {
            list_tasks(Some(project)).await?;
        }
        [command, action, id] if command == "task" && action == "show" => {
            show_task(id).await?;
        }
        [command, action, options @ ..] if command == "task" && action == "create" => {
            create_task(options).await?;
        }
        [command, group, action, id] if command == "task" && group == "dep" && action == "list" => {
            list_dependencies(id).await?;
        }
        [command, group, action, id, blocker]
            if command == "task"
                && group == "dep"
                && matches!(action.as_str(), "add" | "remove") =>
        {
            change_dependency(id, blocker, action == "add").await?;
        }
        [command, action, id, tail @ ..]
            if command == "task" && action == "close" && tail.len() <= 1 =>
        {
            set_task_status(
                id,
                TaskStatus::Closed {
                    commit: tail.first().cloned(),
                },
            )
            .await?;
        }
        [command, action, id] if command == "task" && action == "accept" => {
            accept_assignment(id).await?;
        }
        [command, action, id] if command == "task" && action == "reopen" => {
            set_task_status(id, TaskStatus::Open).await?;
        }
        [command, action] if command == "task" && action == "claims" => {
            list_claims().await?;
        }
        [command, action, id, lease] if command == "task" && action == "renew" => {
            renew_claim(id, lease).await?;
        }
        [command, action, id, lease] if command == "task" && action == "release" => {
            release_claim(id, lease).await?;
        }
        [command, action, id, host, harness, workspace]
            if command == "task" && action == "dispatch" =>
        {
            let harness = match harness.as_str() {
                "claude" => DispatchHarness::Claude,
                "codex" => DispatchHarness::Codex,
                _ => anyhow::bail!("harness must be claude or codex"),
            };
            dispatch_task(id, host, harness, workspace).await?;
        }
        _ => anyhow::bail!("unknown command or arguments; see 'marshal --help'"),
    }
    Ok(())
}

async fn connect() -> Result<MarshalClient> {
    let client = MarshalClient::new();
    marshal_client::config::configure_operator_token(&client)?;
    client.set_address(Some(server_address()));
    await_value(|| {
        matches!(
            client.connection_status().get(),
            ConnectionStatus::Connected(_)
        )
        .then_some(())
    })
    .await
    .context("connecting to marshal-server")?;
    Ok(client)
}

async fn list_tasks(project: Option<&str>) -> Result<()> {
    let client = connect().await?;
    let watch = client.watch_query_state::<GetAllTasks>(GetAllTasks {});
    let mut tasks = await_value(|| {
        if let Some(error) = watch.error().get() {
            Some(Err(anyhow::anyhow!("{error}")))
        } else {
            watch.ready().get().then(|| Ok(watch.items().get()))
        }
    })
    .await??;
    tasks.retain(|task| project.is_none_or(|project| task.project == project));
    tasks.sort_unstable_by(|left, right| {
        (&left.project, left.priority, left.created, &left.id.0).cmp(&(
            &right.project,
            right.priority,
            right.created,
            &right.id.0,
        ))
    });
    println!("{}", serde_json::to_string_pretty(&tasks)?);
    Ok(())
}

async fn show_task(id: &str) -> Result<()> {
    let client = connect().await?;
    let watch = client.watch_report_result::<GetTaskById, Option<Arc<Task>>>(GetTaskById {
        id: TaskId(Arc::from(id)),
    });
    let task = await_value(|| watch.get())
        .await?
        .map_err(anyhow::Error::msg)?
        .context("task not found")?;
    println!("{}", serde_json::to_string_pretty(&task)?);
    Ok(())
}

async fn create_task(args: &[String]) -> Result<()> {
    let (title, flags) = args.split_last().context("task create requires a title")?;
    let (options, remainder) = flags.as_chunks::<2>();
    anyhow::ensure!(remainder.is_empty(), "each task option requires a value");
    let mut project = None;
    let mut body = String::new();
    let mut priority = TaskPriority::default();
    let mut labels = Vec::new();
    for [flag, value] in options {
        match flag.as_str() {
            "--project" => project = Some(value.clone()),
            "--body" => body = value.clone(),
            "--priority" => {
                priority = match value.as_str() {
                    "p0" => TaskPriority::P0,
                    "p1" => TaskPriority::P1,
                    "p2" => TaskPriority::P2,
                    "p3" => TaskPriority::P3,
                    _ => anyhow::bail!("priority must be p0, p1, p2, or p3"),
                }
            }
            "--label" => labels.push(value.clone()),
            _ => anyhow::bail!("unknown task option {flag}"),
        }
    }
    let project = match project {
        Some(project) => project,
        None => infer_project()?,
    };
    let client = connect().await?;
    let response = client.send_command::<CreateTask, TaskId>(&CreateTask {
        project,
        title: title.clone(),
        body,
        priority,
        labels,
    });
    let id = await_value(|| response.get())
        .await?
        .map_err(anyhow::Error::msg)?;
    println!("{id}");
    Ok(())
}

async fn change_dependency(id: &str, blocker: &str, add: bool) -> Result<()> {
    let client = connect().await?;
    let task_id = TaskId(Arc::from(id));
    let blocker_id = TaskId(Arc::from(blocker));
    let response = if add {
        client.send_command::<AddTaskDependency, TaskBlockerId>(&AddTaskDependency {
            task_id,
            blocker_id,
        })
    } else {
        client.send_command::<RemoveTaskDependency, TaskBlockerId>(&RemoveTaskDependency {
            task_id,
            blocker_id,
        })
    };
    let edge = await_value(|| response.get())
        .await?
        .map_err(anyhow::Error::msg)?;
    println!("{edge}");
    Ok(())
}

async fn list_dependencies(id: &str) -> Result<()> {
    let client = connect().await?;
    let watch = client.watch_query_state::<GetAllTaskBlockers>(GetAllTaskBlockers {});
    let mut edges = await_value(|| {
        if let Some(error) = watch.error().get() {
            Some(Err(anyhow::anyhow!("{error}")))
        } else {
            watch.ready().get().then(|| Ok(watch.items().get()))
        }
    })
    .await??;
    edges.retain(|edge| edge.task_id.0.as_ref() == id);
    edges.sort_unstable_by(|a, b| a.blocker_id.0.cmp(&b.blocker_id.0));
    println!("{}", serde_json::to_string_pretty(&edges)?);
    Ok(())
}

fn launch_session() -> Result<(PathBuf, SessionId)> {
    let join = std::env::var_os("MARSHAL_HOST_JOIN_FILE")
        .context("this session is not running under a Marshal launch")?;
    let join = PathBuf::from(join);
    let session: String =
        serde_json::from_slice(&std::fs::read(&join).context("reading launch session identity")?)?;
    anyhow::ensure!(!session.is_empty(), "launch session has not joined yet");
    Ok((join, SessionId(session.into())))
}

fn assignment_proof_from_env() -> Result<Option<String>> {
    if let Some(path) = std::env::var_os("MARSHAL_ASSIGNMENT_PROOF_FILE") {
        Ok(Some(
            std::fs::read_to_string(path).context("reading launch assignment proof")?,
        ))
    } else {
        std::env::var_os("MARSHAL_ASSIGNMENT_PROOF")
            .map(|proof| {
                proof
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid assignment proof"))
            })
            .transpose()
    }
}

async fn accept_assignment(id: &str) -> Result<()> {
    let id = uuid::Uuid::parse_str(id)
        .context("invalid task ID")?
        .to_string();
    let (join, session) = launch_session()?;
    let proof = assignment_proof_from_env()?;
    if let Some(proof) = proof {
        let launch_id =
            marshal_client::hooks::agent_id_from_env().context("missing Marshal launch ID")?;
        let client = connect().await?;
        let response = client.send_command::<AcceptTaskAssignment, ()>(&AcceptTaskAssignment {
            task_id: TaskId(id.into()),
            launch_id,
            session_id: session,
            proof,
        });
        await_value(|| response.get())
            .await?
            .map_err(anyhow::Error::msg)?;
        println!("Assignment accepted.");
        return Ok(());
    }
    write_file(
        &join.with_file_name("accepted-task"),
        &serde_json::to_string(&id)?,
    )?;
    println!("Acceptance recorded. The launch host will submit it to Marshal.");
    Ok(())
}

async fn dispatch_task(
    id: &str,
    host: &str,
    harness: DispatchHarness,
    workspace: &str,
) -> Result<()> {
    let client = connect().await?;
    let response =
        client.send_command::<ReserveTaskDispatch, DispatchAttempt>(&ReserveTaskDispatch {
            task_id: TaskId(Arc::from(id)),
            host_id: host.into(),
            harness,
            workspace: workspace.into(),
        });
    let attempt = await_value(|| response.get())
        .await?
        .map_err(anyhow::Error::msg)?;
    println!("{}", serde_json::to_string(&attempt)?);
    Ok(())
}

async fn list_claims() -> Result<()> {
    let client = connect().await?;
    let watch = client.watch_query_state::<GetAllTaskClaims>(GetAllTaskClaims {});
    let snapshot = await_value(|| {
        if let Some(error) = watch.error().get() {
            Some(Err(anyhow::anyhow!("{error}")))
        } else {
            watch.ready().get().then(|| Ok(watch.items().get()))
        }
    })
    .await??;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let active: Vec<_> = snapshot
        .iter()
        .filter(|claim| u128::try_from(claim.expires).is_ok_and(|expires| expires > now))
        .collect();
    println!("{}", serde_json::to_string_pretty(&active)?);
    Ok(())
}

async fn renew_claim(id: &str, lease: &str) -> Result<()> {
    let client = connect().await?;
    let response = client.send_command::<RenewTaskClaim, TaskClaim>(&RenewTaskClaim {
        task_id: TaskId(Arc::from(id)),
        lease_id: lease.into(),
    });
    let claim = await_value(|| response.get())
        .await?
        .map_err(anyhow::Error::msg)?;
    println!("{}", serde_json::to_string(&claim)?);
    Ok(())
}

async fn release_claim(id: &str, lease: &str) -> Result<()> {
    let client = connect().await?;
    let response = client.send_command::<ReleaseTaskClaim, ()>(&ReleaseTaskClaim {
        task_id: TaskId(Arc::from(id)),
        lease_id: lease.into(),
    });
    await_value(|| response.get())
        .await?
        .map_err(anyhow::Error::msg)
}

async fn set_task_status(id: &str, status: TaskStatus) -> Result<()> {
    let client = connect().await?;
    if let TaskStatus::Closed { commit } = &status
        && let Some(proof) = assignment_proof_from_env()?
    {
        let (_, session_id) = launch_session()?;
        let launch_id =
            marshal_client::hooks::agent_id_from_env().context("missing Marshal launch ID")?;
        let response = client.send_command::<CompleteTaskAssignment, ()>(&CompleteTaskAssignment {
            task_id: TaskId(Arc::from(id)),
            launch_id,
            session_id,
            proof,
            commit: commit.clone(),
        });
        return await_value(|| response.get())
            .await?
            .map_err(anyhow::Error::msg);
    }
    let response = client.send_command::<SetTaskStatus, ()>(&SetTaskStatus {
        id: TaskId(Arc::from(id)),
        status,
    });
    await_value(|| response.get())
        .await?
        .map_err(anyhow::Error::msg)
}

fn infer_project() -> Result<String> {
    marshal_client::config::repository_key(".")
        .context("cannot derive a project from Git origin; use --project")
}

async fn find_coordination(project: Option<String>, query: &[String]) -> Result<()> {
    let client = connect().await?;
    let response =
        client.send_command::<FindCoordination, CoordinationMatches>(&FindCoordination {
            project,
            query: (!query.is_empty()).then(|| query.join(" ")),
        });
    let found = await_value(|| response.get())
        .await?
        .map_err(anyhow::Error::msg)?;
    println!("{}", serde_json::to_string_pretty(&found)?);
    Ok(())
}

async fn list_hosts() -> Result<()> {
    let client = connect().await?;
    let watch = client.watch_query_state::<GetAllDispatchHosts>(GetAllDispatchHosts {});
    let mut hosts = await_value(|| {
        if let Some(error) = watch.error().get() {
            Some(Err(anyhow::anyhow!("{error}")))
        } else {
            watch.ready().get().then(|| Ok(watch.items().get()))
        }
    })
    .await??;
    hosts.sort_unstable_by(|left, right| left.id.0.cmp(&right.id.0));
    println!("{}", serde_json::to_string_pretty(&hosts)?);
    Ok(())
}

async fn list_agents() -> Result<()> {
    let client = connect().await?;
    let watch = client.watch_query_state::<GetAllSessions>(GetAllSessions {});
    let snapshot = await_value(|| {
        if let Some(error) = watch.error().get() {
            Some(Err(anyhow::anyhow!("{error}")))
        } else {
            watch.ready().get().then(|| Ok(watch.items().get()))
        }
    })
    .await??;
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    Ok(())
}

async fn send_messages(session: &str, recipients: &[String], body: &str) -> Result<()> {
    let client = connect().await?;
    let mut failed = false;
    for recipient in recipients {
        let response = client.send_command::<SendMessage, SendMessageResult>(&SendMessage {
            as_session: Some(SessionId(Arc::from(session))),
            to_session_id: SessionId(Arc::from(recipient.as_str())),
            body: body.into(),
            response_required: false,
        });
        match await_value(|| response.get()).await {
            Ok(Ok(result)) => println!("{}", serde_json::to_string(&result)?),
            Ok(Err(error)) => {
                failed = true;
                eprintln!("{recipient}: {error:?}");
            }
            Err(error) => {
                failed = true;
                eprintln!("{recipient}: {error} (delivery uncertain)");
            }
        }
    }
    anyhow::ensure!(!failed, "one or more messages could not be confirmed");
    Ok(())
}

async fn await_value<T>(mut read: impl FnMut() -> Option<T>) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(value) = read() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("timed out waiting for Marshal")
}

fn validate_url(url: &str) -> Result<()> {
    let uri = url.parse::<http::Uri>().context("invalid server URL")?;
    anyhow::ensure!(
        matches!(uri.scheme_str(), Some("ws" | "wss")) && uri.host().is_some(),
        "server URL must use ws:// or wss:// and include a host"
    );
    Ok(())
}

fn config_path(name: &str) -> Result<PathBuf> {
    config_file_candidates(name)
        .into_iter()
        .next()
        .context("no Marshal configuration directory available")
}

fn write_config(name: &str, value: &str) -> Result<()> {
    let path = config_path(name)?;
    write_file(&path, &format!("{value}\n"))?;
    println!("Saved {}", path.display());
    Ok(())
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("configuration path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(content.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

fn install_service(flags: &[String]) -> Result<()> {
    let (pairs, remainder) = flags.as_chunks::<2>();
    anyhow::ensure!(
        remainder.is_empty()
            && pairs.iter().all(|flag| {
                flag[0] == "--launch-harness" && matches!(flag[1].as_str(), "claude" | "codex")
            }),
        "service flags must be --launch-harness claude or --launch-harness codex"
    );
    let binary = std::env::current_exe()?
        .with_file_name(format!("marshal-host{}", std::env::consts::EXE_SUFFIX));
    anyhow::ensure!(
        binary.is_file(),
        "install marshal-host alongside the marshal CLI"
    );
    let mut settings = BTreeMap::new();
    for key in [
        "MARSHAL_HOST_URL",
        "MARSHAL_HOST_ID",
        "MARSHAL_HOST_TOKEN_FILE",
        "MARSHAL_HOST_WORKSPACE_ROOT",
    ] {
        let value = std::env::var(key).ok().filter(|value| !value.is_empty());
        let value = match key {
            "MARSHAL_HOST_URL" => value.or_else(|| {
                std::fs::read_to_string(config_path("host-address").ok()?)
                    .ok()
                    .map(|url| url.trim().to_owned())
            }),
            "MARSHAL_HOST_WORKSPACE_ROOT" if flags.is_empty() => continue,
            _ => value,
        }
        .with_context(|| format!("{key} is required"))?;
        let value = match key {
            "MARSHAL_HOST_URL" => {
                validate_url(&value)?;
                value
            }
            "MARSHAL_HOST_TOKEN_FILE" | "MARSHAL_HOST_WORKSPACE_ROOT" => Path::new(&value)
                .canonicalize()?
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("{key} path is not UTF-8"))?,
            _ => value,
        };
        settings.insert(key, value);
    }
    // Agent binaries are often in a user-specific directory absent from the
    // service manager's inherited PATH.
    if let Ok(path) = std::env::var("PATH") {
        settings.insert("PATH", path);
    }
    let binary = binary.to_str().context("marshal-host path is not UTF-8")?;
    match std::env::consts::OS {
        "linux" => install_systemd(binary, flags, &settings),
        "macos" => install_launch_agent(binary, flags, &settings),
        "windows" => install_scheduled_task(binary, flags, &settings),
        platform => anyhow::bail!("service installation is unavailable on {platform}"),
    }
}

fn install_systemd(
    binary: &str,
    flags: &[String],
    settings: &BTreeMap<&str, String>,
) -> Result<()> {
    let mut unit = String::from("[Unit]\nDescription=Marshal agent host\n\n[Service]\n");
    for (key, value) in settings {
        unit.push_str(&format!(
            "Environment={}\n",
            quote_unit(&format!("{key}={value}"))?
        ));
    }
    unit.push_str(&format!("ExecStart=:{}", quote_unit(binary)?));
    for arg in flags {
        unit.push_str(&format!(" {}", quote_unit(arg)?));
    }
    unit.push_str("\nRestart=on-failure\nRestartSec=3\n\n[Install]\nWantedBy=default.target\n");
    let user_config = config_path("host-address")?;
    let root = user_config
        .parent()
        .and_then(Path::parent)
        .context("missing user config root")?;
    write_file(&root.join("systemd/user/marshal-host.service"), &unit)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "marshal-host.service"])?;
    systemctl(&["restart", "marshal-host.service"])
}

const LAUNCH_LABEL: &str = "rocks.lucid.marshal-host";
const WINDOWS_TASK: &str = "MarshalHost";

fn install_launch_agent(
    binary: &str,
    flags: &[String],
    settings: &BTreeMap<&str, String>,
) -> Result<()> {
    let home = std::env::var_os("HOME").context("HOME is required")?;
    let path = PathBuf::from(home).join(format!("Library/LaunchAgents/{LAUNCH_LABEL}.plist"));
    let mut plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{LAUNCH_LABEL}</string>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ProgramArguments</key><array><string>{}</string>"#,
        xml_text(binary)?
    );
    for arg in flags {
        plist.push_str(&format!("<string>{}</string>", xml_text(arg)?));
    }
    plist.push_str("</array><key>EnvironmentVariables</key><dict>");
    for (key, value) in settings {
        plist.push_str(&format!(
            "<key>{key}</key><string>{}</string>",
            xml_text(value)?
        ));
    }
    plist.push_str("</dict></dict></plist>\n");
    write_file(&path, &plist)?;

    let domain = mac_service_domain()?;
    let target = format!("{domain}/{LAUNCH_LABEL}");
    if std::process::Command::new("launchctl")
        .args(["print", &target])
        .output()?
        .status
        .success()
    {
        run_service_tool("launchctl", &["bootout", &target])?;
    }
    let path = path.to_str().context("LaunchAgent path is not UTF-8")?;
    run_service_tool("launchctl", &["bootstrap", &domain, path])
}

fn mac_service_domain() -> Result<String> {
    let user = std::process::Command::new("id")
        .arg("-u")
        .output()
        .context("looking up user ID")?;
    anyhow::ensure!(user.status.success(), "id -u failed");
    let uid = std::str::from_utf8(&user.stdout)?.trim().parse::<u32>()?;
    Ok(format!("gui/{uid}"))
}

fn install_scheduled_task(
    binary: &str,
    flags: &[String],
    settings: &BTreeMap<&str, String>,
) -> Result<()> {
    let profile = config_path("host-service.json")?;
    write_file(&profile, &serde_json::to_string(settings)?)?;
    let profile = profile.to_str().context("host profile path is not UTF-8")?;
    let args = format!("--profile \"{profile}\" {}", flags.join(" "));
    let user = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .context("looking up the Windows user SID")?;
    anyhow::ensure!(user.status.success(), "whoami /user failed");
    // The account name can use the active Windows code page. Only the SID is ASCII.
    let sid = user
        .stdout
        .rsplit(|&byte| byte == b',')
        .next()
        .context("missing Windows user SID")?;
    let sid = std::str::from_utf8(sid)?.trim().trim_matches('"');
    anyhow::ensure!(
        sid.strip_prefix("S-1-")
            .is_some_and(|suffix| !suffix.is_empty()
                && suffix.bytes().all(|b| b.is_ascii_digit() || b == b'-')),
        "invalid Windows user SID"
    );
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="User"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings>
  <Actions Context="User"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions>
</Task>"#,
        xml_text(binary)?,
        xml_text(&args)?
    );
    let mut file = tempfile::Builder::new().suffix(".xml").tempfile()?;
    file.write_all(&[0xff, 0xfe])?;
    for unit in xml.encode_utf16() {
        file.write_all(&unit.to_le_bytes())?;
    }
    file.flush()?;
    let path = file
        .path()
        .to_str()
        .context("task definition path is not UTF-8")?;
    run_service_tool(
        "schtasks.exe",
        &["/Create", "/TN", WINDOWS_TASK, "/XML", path, "/F"],
    )?;
    // A previous instance may still be using the old profile.
    let _ = std::process::Command::new("schtasks.exe")
        .args(["/End", "/TN", WINDOWS_TASK])
        .output();
    run_service_tool("schtasks.exe", &["/Run", "/TN", WINDOWS_TASK])
}

fn xml_text(value: &str) -> Result<String> {
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "service settings cannot contain control characters"
    );
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;"))
}

fn service_status() -> Result<()> {
    match std::env::consts::OS {
        "linux" => systemctl(&["status", "marshal-host.service"]),
        "macos" => run_service_tool(
            "launchctl",
            &[
                "print",
                &format!("{}/{LAUNCH_LABEL}", mac_service_domain()?),
            ],
        ),
        "windows" => run_service_tool(
            "schtasks.exe",
            &["/Query", "/TN", WINDOWS_TASK, "/FO", "LIST", "/V"],
        ),
        platform => anyhow::bail!("service status is unavailable on {platform}"),
    }
}

fn run_service_tool(program: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("running {program}"))?;
    anyhow::ensure!(status.success(), "{program} exited with {status}");
    Ok(())
}

fn quote_unit(value: &str) -> Result<String> {
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "systemd values cannot contain control characters"
    );
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("running systemctl --user")?;
    anyhow::ensure!(status.success(), "systemctl exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use marshal_client::config::project_from_remote;

    #[test]
    fn project_keys_group_ssh_and_https_checkouts_without_exposing_credentials() {
        assert_eq!(
            project_from_remote("git@github.com:org/repo.git").as_deref(),
            Some("github.com/org/repo")
        );
        assert_eq!(
            project_from_remote("https://user:secret@github.com/org/repo.git?auth=secret")
                .as_deref(),
            Some("github.com/org/repo")
        );
        assert_eq!(
            project_from_remote("ssh://git@forge.example:2222/org/repo.git").as_deref(),
            Some("forge.example:2222/org/repo")
        );
        assert_eq!(project_from_remote("/srv/repos/local.git"), None);
    }
}
