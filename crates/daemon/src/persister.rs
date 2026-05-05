//! Append-only JSONL disk persister.
//!
//! Events are written one per line to `<state_dir>/events.jsonl` with an
//! fsync after every write so a crash or kill -9 still leaves the log in a
//! recoverable state. On startup the file is replayed: events are
//! deduplicated to the latest SET/DEL per `(item_type, item_id)` and
//! surviving SETs are pushed into the server's `StoreRegistry` via
//! `apply_event_batch` with persistence and relationship cascades both
//! suppressed (we are restoring a snapshot — the original cascades already
//! fired at write time).

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use myko::{
    server::{CellServerCtx, PersistError, PersistHealth, Persister},
    wire::{MEvent, MEventType},
};

/// Default state-directory location (`~/.local/state/claude-coord`). The
/// daemon honors `CLAUDE_COORD_STATE_DIR` to override.
pub fn default_state_dir() -> PathBuf {
    if let Ok(s) = std::env::var("CLAUDE_COORD_STATE_DIR") {
        return PathBuf::from(s);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/state/claude-coord");
    }
    PathBuf::from(".claude-coord")
}

pub struct DiskPersister {
    file: Arc<Mutex<File>>,
    path: PathBuf,
    health: Arc<PersistHealth>,
}

impl DiskPersister {
    /// Open (or create) the JSONL log at `path`. The parent directory is
    /// created if missing.
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            path,
            health: Arc::new(PersistHealth::default()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Replay the JSONL log into `ctx`'s store. Call once after
    /// `CellServer::builder().build()` and before `server.run()` so the
    /// registry is hot before any client connects. Returns the number of
    /// entities restored.
    pub fn replay(&self, ctx: &CellServerCtx) -> std::io::Result<usize> {
        let mut file = self.file.lock().expect("disk persister mutex poisoned");
        file.seek(SeekFrom::Start(0))?;

        // Stream the file once; collect by (item_type, id) → latest event.
        // serde_json::from_str is per-line, so a corrupt tail line doesn't
        // poison earlier events.
        let mut latest: HashMap<(String, String), MEvent> = HashMap::new();
        let mut total = 0usize;
        let mut malformed = 0usize;

        let reader = BufReader::new(&*file);
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<MEvent>(&line) {
                Ok(event) => {
                    total += 1;
                    let id = event
                        .item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if id.is_empty() {
                        malformed += 1;
                        continue;
                    }
                    latest.insert((event.item_type.clone(), id), event);
                }
                Err(e) => {
                    malformed += 1;
                    log::warn!("[disk-persister] skipping malformed line: {e}");
                }
            }
        }

        // Position the cursor at end-of-file for subsequent appends.
        file.seek(SeekFrom::End(0))?;
        drop(file);

        // Keep only events whose latest change is SET; mark them
        // replay-only so apply_event_batch doesn't re-persist them or
        // re-process cascade relationships.
        let surviving: Vec<MEvent> = latest
            .into_values()
            .filter(|e| matches!(e.change_type, MEventType::SET))
            .map(|mut e| {
                let mut opts = e.options.clone().unwrap_or_default();
                opts.prevent_persist = true;
                opts.prevent_relationship_updates = true;
                e.options = Some(opts);
                e
            })
            .collect();

        let surviving_count = surviving.len();
        let applied = ctx.apply_event_batch(surviving).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("apply_event_batch: {e}"))
        })?;

        log::info!(
            "[disk-persister] replayed {} events from {} ({} malformed, {} survived dedup, {} applied)",
            total,
            self.path.display(),
            malformed,
            surviving_count,
            applied,
        );

        Ok(applied)
    }
}

impl Persister for DiskPersister {
    fn persist(&self, event: MEvent) -> Result<(), PersistError> {
        self.health.record_enqueue();

        let serialized = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(e) => {
                self.health.record_dropped(e.to_string());
                return Err(PersistError {
                    entity_type: event.item_type,
                    message: format!("serialize: {e}"),
                });
            }
        };

        let mut file = self.file.lock().expect("disk persister mutex poisoned");
        let write_result = file
            .write_all(serialized.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_data());

        if let Err(e) = write_result {
            self.health.record_error(e.to_string());
            return Err(PersistError {
                entity_type: event.item_type,
                message: format!("write/fsync: {e}"),
            });
        }

        self.health.record_success();
        Ok(())
    }

    fn health(&self) -> Arc<PersistHealth> {
        self.health.clone()
    }
}

