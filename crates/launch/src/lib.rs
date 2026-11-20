//! `marshal-launch`: start an agent somewhere and see it join the roster.
//!
//! The shared types come first, so the built-in tmux adapter and the external
//! `marshal-launch-<name>` adapters agree on them.

pub mod address;
pub mod api;
pub mod external;
pub mod first_task;
pub mod intent;
pub mod join;
pub use intent::{
    BackendIntent, LaunchContext, LaunchIntent, LaunchRequest, ManagedIntent, PulseWorkspace,
    WorkspaceIdentity,
};
pub mod record;
pub mod tmux;

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub use external::ExternalBackend;

/// The version of the adapter request/response contract this crate speaks.
pub const PROTOCOL_VERSION: u32 = 3;

/// Resolved process data consumed by process-only backends.
/// The external wire contract is [`LaunchRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRequest {
    /// `claude` or `codex`.
    pub harness: String,
    /// The command to run, as given. Adapters add no flags to it.
    pub argv: Vec<String>,
    /// The folder to run it in, on the target.
    pub cwd: String,
    /// A label for the launch (tmux session name, VM name, ...).
    pub name: String,
    /// The `launch-<uuid>` id the agent's roster row will carry.
    pub agent_id: String,
    /// The launcher's own version. `check` requires the target's `marshal-shim`
    /// to have the same major.minor ([`same_major_minor`]).
    pub marshal_version: String,
    /// Variables to add to the agent's environment. Never secrets.
    pub env: BTreeMap<String, String>,
    /// Adapter-specific options, passed through after `--`.
    pub options: Vec<String>,
}

/// What an adapter reports after starting something.
///
/// Serialized as the JSON object an external adapter writes on stdout.
/// Unknown fields (such as the response's `version`) are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launched {
    /// Opaque to the launcher; the adapter's own way to find the launch again.
    pub handle: String,
    /// The host the agent runs on, as its roster row's `host.name` will show it.
    pub host: String,
    /// The folder the agent runs in, as its roster row's `cwd` will show it.
    pub cwd: String,
    /// Human-readable location, e.g. `build-03 tmux:claude-app`.
    pub r#where: String,
    /// A command that attaches a terminal to the agent, if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReceipt {
    Stopped,
    Gone,
}

/// Starts a worker and manages it by its recovery handle.
/// Marshal observes roster registration separately.
pub trait LaunchBackend: Send + Sync {
    /// The adapter's name, as in `marshal-launch <name> ...`.
    fn name(&self) -> &str;
    /// Confirm the target can run this request, starting nothing.
    fn check(&self, req: &LaunchRequest) -> Result<()>;
    /// Derive a stable status and stop handle before starting the worker.
    fn recovery_handle(&self, req: &LaunchRequest) -> Result<String>;
    /// Start the selected intent and return its resolved worker location.
    fn launch(&self, req: &LaunchRequest) -> Result<Launched>;
    /// Observe launch metadata using the stable handle after a lost start response.
    fn recover(&self, _req: &LaunchRequest) -> Result<Option<Launched>> {
        Ok(None)
    }
    /// Stop what `handle` names.
    fn stop(&self, handle: &str) -> Result<StopReceipt>;
    /// Whether what `handle` names is still running; `None` if the adapter can't tell.
    fn status(&self, handle: &str) -> Result<Option<bool>>;
}

pub fn backend_for(name: &str) -> Result<Box<dyn LaunchBackend>> {
    match name {
        address::LOCAL_BACKEND => Ok(Box::new(tmux::Tmux::default())),
        other => Ok(Box::new(ExternalBackend::find(other)?)),
    }
}

/// This build's version, sent as the request's `marshal_version`.
pub const MARSHAL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether two versions share major.minor. Marshal releases every crate at one
/// version, so that is what a target's shim must match.
pub fn same_major_minor(a: &str, b: &str) -> bool {
    let mm = |v: &str| {
        let mut it = v.trim().trim_start_matches('v').split('.');
        match (it.next(), it.next()) {
            (Some(major), Some(minor)) if !major.is_empty() && !minor.is_empty() => Some((
                major.to_string(),
                minor.split(['-', '+']).next().unwrap_or("").to_string(),
            )),
            _ => None,
        }
    };
    matches!((mm(a), mm(b)), (Some(x), Some(y)) if x == y)
}

/// Check `marshal-shim --version` output (`marshal-shim 0.60.2`) against the
/// request's `marshal_version`, and say plainly what's wrong if it doesn't match.
pub fn check_shim_version(output: &str, wanted: &str) -> Result<()> {
    let found = output
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or("");
    if found.is_empty() {
        bail!(
            "marshal-shim --version printed {:?}, which has no version in it",
            output.trim()
        );
    }
    if !same_major_minor(found, wanted) {
        bail!(
            "the target's marshal-shim is {found}, but this launcher is {wanted}: they must share \
             major.minor, so nothing was started. Install a matching marshal-shim there"
        );
    }
    Ok(())
}

/// A fresh id for one launch. No earlier row can carry it.
pub fn new_agent_id() -> String {
    format!("launch-{}", uuid::Uuid::new_v4().simple())
}

/// The command a harness starts with. It takes no flags: the agent starts in
/// the harness's default permission mode. Codex goes through `codex-run`,
/// which gives an idle Codex thread live wakes.
pub fn harness_argv(harness: &str) -> Result<Vec<String>> {
    match harness {
        "claude" => Ok(vec!["claude".into()]),
        "codex" => Ok(vec!["marshal-shim".into(), "codex-run".into()]),
        other => bail!("unknown harness {other:?}: marshal-launch starts claude or codex"),
    }
}

/// Settings passed from the launcher's environment to any target. None is a secret.
/// `MARSHAL_BASE_URL` is the daemon's hook listener when it isn't on port 6156 of
/// the launch address's host. It travels with the launch address, and a
/// non-local adapter is refused a loopback one ([`address::check_hook_base_for_backend`]).
const PASSED_ENV: &[&str] = &[
    "MARSHAL_OPERATOR",
    "MARSHAL_WAITER_LOCK_WAIT_SECS",
    "MARSHAL_BASE_URL",
];

/// Settings that only mean something on this machine, passed to the local
/// adapter only: a running tmux server would otherwise give the agent the
/// server's values. None is a secret, and none names an identity.
const LOCAL_ENV: &[&str] = &[
    "MARSHAL_HOST",
    "MARSHAL_BIN_DIR",
    "MARSHAL_CODEX_BIN",
    // A test's guard against the default daemon, passed only if set.
    "MARSHAL_TEST",
    // Where the shim reads its daemon-address file from.
    "XDG_CONFIG_HOME",
];

/// The agent's added environment: the launch address, this launch's id (never
/// the launcher's own), and the listed settings that are set here.
pub fn launch_env(
    get: impl Fn(&str) -> Option<String>,
    local: bool,
    launch_address: &str,
    agent_id: &str,
) -> BTreeMap<String, String> {
    let names = PASSED_ENV.iter().chain(if local { LOCAL_ENV } else { &[] });
    let mut env: BTreeMap<String, String> = names
        .filter_map(|k| get(k).map(|v| (k.to_string(), v)))
        .collect();
    env.insert("MARSHAL_DAEMON_ADDRESS".into(), launch_address.into());
    env.insert("MARSHAL_AGENT_ID".into(), agent_id.into());
    env
}

/// What `marshal-launch <adapter> <harness>` was asked for.
#[derive(Debug, Clone)]
pub struct Plan {
    pub harness: String,
    pub cwd: String,
    pub name: String,
    /// Where the agent finds the daemon.
    pub launch_address: String,
    /// Where the launcher itself reaches the same daemon, to wait for the join
    /// and send the first task. The binary uses the launch address. A library
    /// caller may give another route to the same daemon.
    pub daemon_address: String,
    pub timeout: Duration,
    pub check: bool,
    pub options: Vec<String>,
    /// Sent to the joined session as one direct message.
    pub task: Option<String>,
    /// Where to record the launch (`launches.json`); `None` records nothing.
    pub records: Option<std::path::PathBuf>,
}

/// A launch that joined.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub agent_id: String,
    pub launched: Launched,
    pub joined: join::Joined,
    /// The first task's message id, if a task was sent.
    pub task_message_id: Option<String>,
}

fn prepare(adapter: &dyn LaunchBackend, plan: &Plan, agent_id: &str) -> Result<LaunchRequest> {
    let local = adapter.name() == address::LOCAL_BACKEND;
    address::check_for_backend(adapter.name(), &plan.launch_address)?;
    let mut req = ProcessRequest {
        harness: plan.harness.clone(),
        argv: harness_argv(&plan.harness)?,
        cwd: plan.cwd.clone(),
        name: plan.name.clone(),
        agent_id: agent_id.into(),
        marshal_version: MARSHAL_VERSION.into(),
        env: launch_env(
            |k| std::env::var(k).ok(),
            local,
            &plan.launch_address,
            agent_id,
        ),
        options: plan.options.clone(),
    };
    if req.harness == "codex" {
        req.env.remove("MARSHAL_AGENT_ID");
        req.env.insert("MARSHAL_LAUNCH_ID".into(), agent_id.into());
    }
    address::check_hook_base_for_backend(
        adapter.name(),
        req.env.get("MARSHAL_BASE_URL").map(String::as_str),
    )?;
    Ok(req.into())
}

/// Launch `plan` through `adapter` and wait for it to join. Nothing starts if
/// the launch address or hook base is loopback for a non-local adapter, if the daemon can't
/// be reached, or if the adapter's `check` fails.
pub async fn launch(adapter: &dyn LaunchBackend, plan: &Plan) -> Result<Outcome> {
    let agent_id = new_agent_id();
    let req = prepare(adapter, plan, &agent_id)?;
    let local = adapter.name() == address::LOCAL_BACKEND;
    // The address checks above need no daemon, so a refused launch fails at once.
    let roster = join::Roster::connect(&plan.daemon_address).await?;
    if plan.check {
        adapter
            .check(&req)
            .with_context(|| format!("the {} adapter's check failed", adapter.name()))?;
        if local {
            // The agent runs here, so this machine's view of both ports is the agent's.
            let hook_base = req.context.hook_base.as_deref();
            address::probe(&plan.launch_address, hook_base, Duration::from_secs(3))?;
        }
    }
    let launched = adapter
        .launch(&req)
        .with_context(|| format!("the {} backend did not confirm startup", adapter.name()))?;
    let store = plan.records.as_ref().map(record::Store::at);
    if let Some(store) = &store {
        // Recorded before the wait, so a launch that never joins can be stopped.
        let entry = record::Launch {
            adapter: adapter.name().into(),
            handle: launched.handle.clone(),
            agent_id: agent_id.clone(),
            name: plan.name.clone(),
            harness: plan.harness.clone(),
            host: launched.host.clone(),
            cwd: launched.cwd.clone(),
            r#where: launched.r#where.clone(),
            attach: launched.attach.clone(),
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            daemon: Some(plan.daemon_address.clone()),
            session_id: None,
        };
        if let Err(e) = store.add(entry) {
            eprintln!(
                "warning: the launch wasn't recorded in {}: {e:#}",
                store.path().display()
            );
        }
    }
    eprintln!(
        "started `{}` at {}; waiting up to {}s for it to join",
        plan.harness,
        launched.r#where,
        plan.timeout.as_secs()
    );
    let joined = roster.wait(&agent_id, &launched, plan.timeout).await?;
    if let Some(store) = &store
        && let Err(e) = store.joined(&agent_id, &joined.session_id)
    {
        eprintln!("warning: {}: {e:#}", store.path().display());
    }
    let task_message_id = match &plan.task {
        None => None,
        Some(task) => {
            let operator = std::env::var("MARSHAL_OPERATOR").ok();
            let host = tmux::this_host();
            let body = first_task::labelled(task, &host, operator.as_deref(), &agent_id);
            let id = first_task::send_for_launch(
                &roster,
                &joined.session_id,
                body,
                &host,
                &agent_id,
            )
            .await
            .with_context(|| {
                format!(
                    "{} ({}) joined, but its first task wasn't sent. It's still running at {}",
                    joined.nickname, joined.session_id, launched.r#where
                )
            })?;
            Some(id)
        }
    };
    Ok(Outcome {
        agent_id,
        launched,
        joined,
        task_message_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn launch_request_serializes_with_the_contract_names() {
        let req = LaunchRequest::from(crate::ProcessRequest {
            harness: "claude".into(),
            argv: vec!["claude".into()],
            cwd: "~/work/app".into(),
            name: "claude-app".into(),
            agent_id: "launch-6f1c".into(),
            marshal_version: "0.60.2".into(),
            env: BTreeMap::from([("MARSHAL_AGENT_ID".into(), "launch-6f1c".into())]),
            options: vec!["--host".into(), "build-03".into()],
        });
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            v,
            json!({
                "version": PROTOCOL_VERSION,
                "name": "claude-app",
                "launch_id": "launch-6f1c",
                "marshal_version": "0.60.2",
                "context": {"daemon_address": "", "hook_base": null},
                "intent": {
                    "kind": "process",
                    "harness": "claude",
                    "argv": ["claude"],
                    "cwd": "~/work/app",
                    "env": {"MARSHAL_AGENT_ID": "launch-6f1c"},
                    "options": ["--host", "build-03"]
                }
            })
        );
        assert_eq!(serde_json::from_value::<LaunchRequest>(v).unwrap(), req);
    }

    #[test]
    fn the_agent_gets_the_launch_address_and_this_launchs_id_and_never_the_launchers() {
        let get = |k: &str| match k {
            "MARSHAL_AGENT_ID" => Some("the-launcher".to_string()),
            "MARSHAL_DAEMON_ADDRESS" => Some("ws://localhost:6155".to_string()),
            "MARSHAL_CODEX_APP_SERVER_SOCKET" | "XDG_CONFIG_HOME" | "MARSHAL_OPERATOR" => {
                Some(format!("{k}-value"))
            }
            "MARSHAL_BASE_URL" => Some("http://marshal.mesh:26156".to_string()),
            _ => None,
        };
        let id = new_agent_id();
        let env = launch_env(get, false, "ws://marshal.mesh:6155", &id);
        assert_eq!(
            env.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "MARSHAL_AGENT_ID",
                "MARSHAL_BASE_URL",
                "MARSHAL_DAEMON_ADDRESS",
                "MARSHAL_OPERATOR"
            ]
        );
        // A non-default hook port travels with the launch address to any adapter.
        assert_eq!(env["MARSHAL_BASE_URL"], "http://marshal.mesh:26156");
        assert_eq!(env["MARSHAL_AGENT_ID"], id);
        assert_eq!(env["MARSHAL_DAEMON_ADDRESS"], "ws://marshal.mesh:6155");
        let local = launch_env(get, true, "ws://localhost:6155", &id);
        assert!(local.contains_key("XDG_CONFIG_HOME"));
        assert!(!local.keys().any(|k| k.contains("APP_SERVER")));
        assert!(id.starts_with("launch-") && id.len() == "launch-".len() + 32);
        assert_ne!(new_agent_id(), id);
    }

    #[test]
    fn the_target_shim_must_share_major_minor() {
        assert!(same_major_minor("0.60.2", "0.60.9"));
        assert!(same_major_minor("v1.2.3-rc1", "1.2.0"));
        assert!(same_major_minor("0.60", "0.60.1"));
        assert!(!same_major_minor("0.58.1", "0.60.2"));
        assert!(!same_major_minor("1.60.2", "0.60.2"));
        assert!(!same_major_minor("0", "0.60.2"));
        assert!(!same_major_minor("", ""));
        check_shim_version("marshal-shim 0.60.5\n", "0.60.2").unwrap();
        let err = check_shim_version("marshal-shim 0.58.1", "0.60.2")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("0.58.1") && err.contains("nothing was started"),
            "{err}"
        );
        assert!(check_shim_version("command not found", "0.60.2").is_err());
    }

    #[test]
    fn harnesses_start_with_no_flags() {
        assert_eq!(harness_argv("claude").unwrap(), ["claude"]);
        assert_eq!(
            harness_argv("codex").unwrap(),
            ["marshal-shim", "codex-run"]
        );
        assert!(harness_argv("bash").is_err());
    }

    #[test]
    fn launched_reads_an_adapter_response_and_names_where() {
        let l: Launched = serde_json::from_value(json!({
            "version": PROTOCOL_VERSION,
            "handle": "h1",
            "host": "build-03",
            "cwd": "/home/u/work/app",
            "where": "build-03 tmux:claude-app",
        }))
        .unwrap();
        assert_eq!(l.r#where, "build-03 tmux:claude-app");
        assert_eq!(l.attach, None);
        let v = serde_json::to_value(&l).unwrap();
        assert!(v.get("where").is_some());
        assert!(v.get("attach").is_none());
    }
}
