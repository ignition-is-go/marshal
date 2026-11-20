//! `launches.json`: what this machine's `marshal-launch` started, for `list`
//! and `stop`. Kept in `$XDG_STATE_HOME/marshal/launches.json`
//! (`~/.local/state/marshal/launches.json` without it).
//!
//! A launch is recorded as soon as the adapter starts it, before the wait, so
//! one that never joins can still be listed and stopped.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// One launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    pub adapter: String,
    pub handle: String,
    pub agent_id: String,
    pub name: String,
    pub harness: String,
    pub host: String,
    pub cwd: String,
    pub r#where: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<String>,
    /// Milliseconds since the epoch.
    pub started_at: i64,
    /// The daemon the launch was made against (its launch address), which
    /// `list` reads the roster from. Absent in records written before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<String>,
    /// The joined session's id, once it joined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    launches: Vec<Launch>,
}

/// Where launches are kept.
pub fn default_path() -> Result<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })
        .context("neither XDG_STATE_HOME nor HOME is set, so launches can't be recorded")?;
    Ok(state.join("marshal").join("launches.json"))
}

/// The launch record file, read and written under a lock file so two launches
/// at once don't drop each other's entries.
pub struct Store {
    path: PathBuf,
}

const LOCK_WAIT: Duration = Duration::from_secs(5);
/// A lock older than this was left by a launcher that died holding it.
const LOCK_STALE: Duration = Duration::from_secs(30);

struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Store {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn open_default() -> Result<Self> {
        Ok(Self::at(default_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> Result<Lock> {
        let dir = self.path.parent().context("launches.json has no folder")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock = self.path.with_extension("json.lock");
        let started = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(_) => return Ok(Lock(lock)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if stale {
                        let _ = std::fs::remove_file(&lock);
                        continue;
                    }
                    if started.elapsed() > LOCK_WAIT {
                        bail!("{} is held by another marshal-launch", lock.display());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e).with_context(|| format!("creating {}", lock.display())),
            }
        }
    }

    fn read(&self) -> Result<File> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) if s.trim().is_empty() => Ok(File::default()),
            Ok(s) => serde_json::from_str(&s)
                .with_context(|| format!("{} isn't a launch record", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn write(&self, file: &File) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(serde_json::to_string_pretty(file)?.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))
    }

    /// Every recorded launch, oldest first.
    pub fn list(&self) -> Result<Vec<Launch>> {
        Ok(self.read()?.launches)
    }

    /// Record a launch, replacing any with the same agent id.
    pub fn add(&self, launch: Launch) -> Result<()> {
        let _lock = self.lock()?;
        let mut f = self.read()?;
        f.launches.retain(|l| l.agent_id != launch.agent_id);
        f.launches.push(launch);
        self.write(&f)
    }

    /// Note the session a launch joined as.
    pub fn joined(&self, agent_id: &str, session_id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut f = self.read()?;
        for l in f.launches.iter_mut().filter(|l| l.agent_id == agent_id) {
            l.session_id = Some(session_id.to_string());
        }
        self.write(&f)
    }

    /// Forget a launch.
    pub fn remove(&self, agent_id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut f = self.read()?;
        f.launches.retain(|l| l.agent_id != agent_id);
        self.write(&f)
    }
}

/// The launch `key` names: an agent id, a name, or a joined session id. A name
/// held by more than one launch is refused rather than guessed.
pub fn find<'a>(launches: &'a [Launch], key: &str) -> Result<&'a Launch> {
    if let Some(l) = launches
        .iter()
        .find(|l| l.agent_id == key || l.session_id.as_deref() == Some(key))
    {
        return Ok(l);
    }
    let named: Vec<&Launch> = launches.iter().filter(|l| l.name == key).collect();
    match named.as_slice() {
        [one] => Ok(one),
        [] => bail!("no recorded launch named {key:?}: see `marshal-launch list`"),
        many => bail!(
            "{key:?} names {} launches ({}); use the agent id",
            many.len(),
            many.iter()
                .map(|l| l.agent_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(agent_id: &str, name: &str) -> Launch {
        Launch {
            adapter: "tmux".into(),
            handle: name.into(),
            agent_id: agent_id.into(),
            name: name.into(),
            harness: "claude".into(),
            host: "build-03".into(),
            cwd: "/w".into(),
            r#where: format!("build-03 tmux:{name}"),
            attach: None,
            started_at: 1,
            daemon: None,
            session_id: None,
        }
    }

    #[test]
    fn find_by_id_name_or_session_and_refuse_a_shared_name() {
        let mut ls = vec![launch("launch-1", "a"), launch("launch-2", "b")];
        ls[1].session_id = Some("sess-2".into());
        assert_eq!(find(&ls, "launch-1").unwrap().name, "a");
        assert_eq!(find(&ls, "b").unwrap().agent_id, "launch-2");
        assert_eq!(find(&ls, "sess-2").unwrap().agent_id, "launch-2");
        assert!(find(&ls, "c").is_err());
        ls.push(launch("launch-3", "a"));
        assert!(
            find(&ls, "a")
                .unwrap_err()
                .to_string()
                .contains("launch-1, launch-3")
        );
    }
}
