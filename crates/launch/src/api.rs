//! The Marshal launch API and its service-owned recovery records.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{
    LaunchBackend, LaunchRequest, Launched, Plan, StopReceipt, address, backend_for, first_task,
    join,
};

pub const LAUNCH_PATH: &str = "/launch/v1/start";
pub const GET_LAUNCH_PATH: &str = "/launch/v1/get";
pub const STOP_LAUNCH_PATH: &str = "/launch/v1/stop";
pub const LIST_LAUNCHES_PATH: &str = "/launch/v1/list";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct StartLaunch {
    pub launch_id: String,
    pub backend: String,
    pub harness: String,
    pub cwd: String,
    pub name: String,
    pub options: Vec<String>,
    pub check: bool,
    pub timeout_secs: u64,
    pub first_task: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct LaunchKey {
    pub launch_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    NotStarted,
    Running,
    Absent,
    Exited,
    Unknown,
    Stopped,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum TaskState {
    NotRequested,
    Pending,
    Stored { message_id: String },
    Acknowledged { message_id: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LaunchView {
    pub launch_id: String,
    pub backend: String,
    pub name: String,
    pub process: ProcessState,
    pub launched: Option<Launched>,
    pub session_id: Option<String>,
    pub task: TaskState,
    pub error: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Record {
    request: StartLaunch,
    backend_request: LaunchRequest,
    handle: String,
    attempted: bool,
    #[serde(default)]
    stop_requested: bool,
    view: LaunchView,
}

pub struct Service {
    directory: PathBuf,
    daemon_address: String,
    launch_address: String,
    hook_base: Option<String>,
    active: Mutex<HashMap<String, watch::Sender<bool>>>,
}

struct Operation<'a> {
    active: &'a Mutex<HashMap<String, watch::Sender<bool>>>,
    id: &'a str,
    cancelled: watch::Receiver<bool>,
}

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        self.active.lock().unwrap().remove(self.id);
    }
}

impl Service {
    pub fn new(
        directory: PathBuf,
        daemon_address: String,
        launch_address: String,
        hook_base: Option<String>,
    ) -> Self {
        Self {
            directory,
            daemon_address,
            launch_address,
            hook_base,
            active: Mutex::default(),
        }
    }

    fn begin<'a>(&'a self, id: &'a str) -> Result<Operation<'a>> {
        let mut active = self.active.lock().unwrap();
        ensure!(
            !active.contains_key(id),
            "launch {id} already has an operation in progress"
        );
        let (signal, cancelled) = watch::channel(false);
        active.insert(id.into(), signal);
        Ok(Operation {
            active: &self.active,
            id,
            cancelled,
        })
    }

    async fn begin_stop<'a>(&'a self, id: &'a str) -> Operation<'a> {
        loop {
            if let Ok(operation) = self.begin(id) {
                return operation;
            }
            if let Some(active) = self.active.lock().unwrap().get(id) {
                let _ = active.send(true);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        ensure!(
            id.strip_prefix("launch-").is_some_and(|key| {
                key.len() == 32
                    && key
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            }) && marshal_entities::launch_task_message_id(id).is_some(),
            "invalid launch ID"
        );
        Ok(self.directory.join(format!("{id}.json")))
    }

    fn load(path: &Path) -> Result<Option<Record>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn save(path: &Path, entry: &Record) -> Result<()> {
        use std::io::Write as _;
        std::fs::create_dir_all(path.parent().context("launch record has no parent")?)?;
        let tmp = path.with_extension("tmp");
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&serde_json::to_vec(&entry)?)?;
        file.sync_all()?;
        std::fs::rename(tmp, path)?;
        #[cfg(unix)]
        std::fs::File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    }

    pub async fn start(&self, request: StartLaunch) -> Result<LaunchView> {
        let path = self.path(&request.launch_id)?;
        let mut operation = self.begin(&request.launch_id)?;
        ensure!(request.timeout_secs > 0, "join timeout must be positive");
        let backend = backend_for(&request.backend)?;
        let mut entry = match Self::load(&path)? {
            Some(entry) => {
                ensure!(
                    entry.request == request,
                    "launch ID already names a different request"
                );
                entry
            }
            None => {
                let plan = Plan {
                    harness: request.harness.clone(),
                    cwd: request.cwd.clone(),
                    name: request.name.clone(),
                    launch_address: self.launch_address.clone(),
                    daemon_address: self.daemon_address.clone(),
                    timeout: Duration::from_secs(request.timeout_secs),
                    check: request.check,
                    options: request.options.clone(),
                    task: request.first_task.clone(),
                    records: None,
                };
                let mut backend_request =
                    crate::prepare(backend.as_ref(), &plan, &request.launch_id)?;
                backend_request.env.remove("MARSHAL_BASE_URL");
                backend_request.env.remove("MARSHAL_OPERATOR");
                if let Some(base) = &self.hook_base {
                    backend_request
                        .env
                        .insert("MARSHAL_BASE_URL".into(), base.clone());
                }
                address::check_hook_base_for_backend(
                    backend.name(),
                    backend_request
                        .env
                        .get("MARSHAL_BASE_URL")
                        .map(String::as_str),
                )?;
                let handle = backend.recovery_handle(&backend_request)?;
                ensure!(
                    !handle.is_empty(),
                    "backend returned an empty recovery handle"
                );
                let view = LaunchView {
                    launch_id: request.launch_id.clone(),
                    backend: request.backend.clone(),
                    name: request.name.clone(),
                    process: ProcessState::NotStarted,
                    launched: None,
                    session_id: None,
                    task: if request.first_task.is_some() {
                        TaskState::Pending
                    } else {
                        TaskState::NotRequested
                    },
                    error: None,
                };
                let entry = Record {
                    request: request.clone(),
                    backend_request,
                    handle,
                    attempted: false,
                    stop_requested: false,
                    view,
                };
                Self::save(&path, &entry)?;
                entry
            }
        };
        if entry.stop_requested {
            return Ok(entry.view);
        }
        entry.view.error = None;
        let result = tokio::select! {
            biased;
            _ = operation.cancelled.wait_for(|value| *value) => {
                Err(anyhow::anyhow!("stop requested while launch was in progress"))
            }
            result = self.advance(&path, &mut entry, backend.as_ref()) => result,
        };
        if let Err(error) = result {
            entry.view.error = Some(format!("{error:#}"));
        }
        Self::save(&path, &entry)?;
        Ok(entry.view)
    }

    async fn advance(
        &self,
        path: &Path,
        entry: &mut Record,
        backend: &dyn LaunchBackend,
    ) -> Result<()> {
        if !entry.attempted {
            if entry.request.check {
                backend.check(&entry.backend_request)?;
                if backend.name() == address::LOCAL_BACKEND {
                    address::probe(
                        &entry.backend_request.env["MARSHAL_DAEMON_ADDRESS"],
                        entry
                            .backend_request
                            .env
                            .get("MARSHAL_BASE_URL")
                            .map(String::as_str),
                        Duration::from_secs(3),
                    )?;
                }
            }
            entry.attempted = true;
            entry.view.process = ProcessState::Unknown;
            Self::save(path, entry)?;
            let launched = backend.launch(&entry.backend_request)?;
            ensure!(
                launched.handle == entry.handle,
                "backend changed the recovery handle"
            );
            entry.view.launched = Some(launched);
            entry.view.process = ProcessState::Running;
            Self::save(path, entry)?;
        } else {
            Self::observe(entry, backend)?;
            if entry.view.launched.is_none()
                && entry.view.process != ProcessState::Absent
                && let Some(launched) = backend.recover(&entry.backend_request)?
            {
                ensure!(
                    launched.handle == entry.handle,
                    "backend changed the recovery handle"
                );
                entry.view.launched = Some(launched);
                entry.view.process = ProcessState::Running;
                Self::save(path, entry)?;
            }
        }
        let Some(launched) = &entry.view.launched else {
            anyhow::bail!(
                "startup was not confirmed, last observed state is {:?}; no second start was attempted",
                entry.view.process
            );
        };
        if matches!(
            entry.view.process,
            ProcessState::Absent | ProcessState::Exited
        ) {
            return Ok(());
        }
        if entry.view.session_id.is_some() && entry.view.task != TaskState::Pending {
            return Ok(());
        }
        let roster = join::Roster::connect(&self.daemon_address).await?;
        if entry.view.session_id.is_none() {
            let joined = roster
                .wait(
                    &entry.request.launch_id,
                    launched,
                    Duration::from_secs(entry.request.timeout_secs),
                )
                .await?;
            entry.view.session_id = Some(joined.session_id);
            Self::save(path, entry)?;
        }
        if let Some(task) = &entry.request.first_task {
            let host = crate::tmux::this_host();
            let body = first_task::labelled(task, &host, None, &entry.request.launch_id);
            let message_id = first_task::send_for_launch(
                &roster,
                entry.view.session_id.as_deref().unwrap(),
                body,
                &host,
                &entry.request.launch_id,
            )
            .await?;
            entry.view.task = TaskState::Stored { message_id };
        }
        Ok(())
    }

    fn observe(entry: &mut Record, backend: &dyn LaunchBackend) -> Result<()> {
        entry.view.process = ProcessState::Unknown;
        entry.view.process = match backend.status(&entry.handle)? {
            Some(true) => ProcessState::Running,
            Some(false) if entry.view.launched.is_some() => ProcessState::Exited,
            Some(false) => ProcessState::Absent,
            None => ProcessState::Unknown,
        };
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<LaunchView> {
        let mut entry = Self::load(&self.path(id)?)?.context("launch not found")?;
        if entry.attempted && entry.view.process != ProcessState::Stopped {
            let result = backend_for(&entry.request.backend)
                .and_then(|backend| Self::observe(&mut entry, backend.as_ref()));
            if let Err(error) = result {
                entry.view.process = ProcessState::Unknown;
                entry.view.error = Some(format!("{error:#}"));
            }
        }
        Ok(entry.view)
    }

    pub fn list(&self) -> Result<Vec<LaunchView>> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut launches = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("json")
                && let Some(entry) = Self::load(&path)?
            {
                launches.push(entry.view);
            }
        }
        launches.sort_by(|a, b| a.launch_id.cmp(&b.launch_id));
        Ok(launches)
    }

    pub async fn stop(&self, id: &str) -> Result<LaunchView> {
        let path = self.path(id)?;
        let _operation = self.begin_stop(id).await;
        let mut entry = Self::load(&path)?.context("launch not found")?;
        if entry.view.process == ProcessState::Stopped {
            return Ok(entry.view);
        }
        entry.stop_requested = true;
        Self::save(&path, &entry)?;
        let receipt = if entry.attempted {
            backend_for(&entry.request.backend).and_then(|backend| backend.stop(&entry.handle))
        } else {
            Ok(StopReceipt::Stopped)
        };
        let error = match receipt {
            Ok(StopReceipt::Gone) if entry.view.launched.is_none() => Some(
                "no worker was found, but startup was never confirmed; retry stop if it appears"
                    .into(),
            ),
            Ok(StopReceipt::Stopped | StopReceipt::Gone) => None,
            Err(error) => Some(format!("{error:#}")),
        };
        entry.view.process = if error.is_some() {
            ProcessState::Unknown
        } else {
            ProcessState::Stopped
        };
        entry.view.error = error;
        Self::save(&path, &entry)?;
        Ok(entry.view)
    }

    pub fn record_task_receipt(&self, id: &str, session: &str, task: TaskState) -> Result<()> {
        let path = self.path(id)?;
        let _operation = self.begin(id)?;
        let mut entry = Self::load(&path)?.context("launch not found")?;
        if entry.view.session_id.as_deref() != Some(session) {
            return Ok(());
        }
        if matches!(
            (&entry.view.task, &task),
            (
                TaskState::Pending,
                TaskState::Stored { .. } | TaskState::Acknowledged { .. }
            ) | (TaskState::Stored { .. }, TaskState::Acknowledged { .. })
        ) {
            entry.view.task = task;
            Self::save(&path, &entry)?;
        }
        Ok(())
    }
}

pub fn service_url(daemon: &str) -> Result<String> {
    hook_url(daemon, address::HOOK_PORT)
}

pub fn hook_url(daemon: &str, hook_port: u16) -> Result<String> {
    let (host, _) = address::host_port(daemon)?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    Ok(format!("http://{host}:{hook_port}"))
}

pub async fn call<Request: Serialize + ?Sized, Response: serde::de::DeserializeOwned>(
    base: &str,
    path: &str,
    request: &Request,
) -> Result<Response> {
    let response = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20 * 60))
        .build()?
        .post(format!("{}{path}", base.trim_end_matches('/')))
        .json(request)
        .send()
        .await?;
    let status = response.status();
    let body = response.bytes().await?;
    ensure!(
        status.is_success(),
        "launch API {status}: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).context("decoding launch API response")
}
