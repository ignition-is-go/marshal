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
//!
//! After replay, an optional filesystem watcher tails the log: any
//! externally-appended lines (e.g. a hand-edit migration against a running
//! daemon) are picked up, deserialized, and dispatched into the live store
//! "as if just emitted internally" — but with `prevent_persist = true` so
//! the writer doesn't echo them back to disk and trigger a feedback loop.
//! See `start_watcher` for details.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use myko::{
    server::{CellServerCtx, PersistError, PersistHealth, Persister},
    wire::{MEvent, MEventType},
};
use notify_debouncer_mini::{
    DebounceEventResult, Debouncer, new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
};

/// Default state-directory location (`~/.local/state/marshal`). The
/// daemon honors `MARSHAL_STATE_DIR` to override.
pub fn default_state_dir() -> PathBuf {
    if let Ok(s) = std::env::var("MARSHAL_STATE_DIR") {
        return PathBuf::from(s);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/state/marshal");
    }
    PathBuf::from(".marshal")
}

/// One-time JSONL persistence migration from the previous claude-coord
/// path. Called once at startup before the persister opens its file:
///
/// - If the old default (`~/.local/state/claude-coord/events.jsonl`) or
///   `$CLAUDE_COORD_STATE_DIR/events.jsonl` exists AND the new path does
///   not, move the old file to the new path. Idempotent: a second run
///   sees the new file in place and no-ops.
/// - If both exist, leave both alone and warn — preferring the new path,
///   so we never clobber state the user has already started writing under
///   the marshal name.
/// - If only the new (or neither) exists, no-op.
///
/// Cross-filesystem moves are not handled: the old and new defaults are
/// both under `~/.local/state` so a `rename(2)` succeeds in practice. A
/// failure here logs and returns the error; the daemon will then open
/// the (still-empty) new path and start fresh.
pub fn migrate_from_claude_coord(new_log_path: &Path) -> std::io::Result<()> {
    let Some(old_log) = legacy_log_path() else {
        return Ok(());
    };
    if !old_log.exists() {
        return Ok(());
    }
    if new_log_path.exists() {
        log::warn!(
            "[migrate] both legacy ({}) and current ({}) event logs exist; \
             leaving both in place. Reconcile manually if needed — current \
             is preferred.",
            old_log.display(),
            new_log_path.display(),
        );
        return Ok(());
    }
    if let Some(parent) = new_log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    log::info!(
        "[migrate] moving event log: {} → {}",
        old_log.display(),
        new_log_path.display(),
    );
    std::fs::rename(&old_log, new_log_path)?;
    Ok(())
}

/// Resolve the previous claude-coord-era event log path, if discoverable.
/// Honors the historical `CLAUDE_COORD_STATE_DIR` override before falling
/// back to `~/.local/state/claude-coord`.
fn legacy_log_path() -> Option<PathBuf> {
    if let Ok(s) = std::env::var("CLAUDE_COORD_STATE_DIR") {
        return Some(PathBuf::from(s).join("events.jsonl"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/state/claude-coord/events.jsonl"))
}

/// File handle plus the bookkeeping needed by the watcher to distinguish
/// the daemon's own appends from external edits. Held under a single
/// mutex so persist() and tail_apply() can't interleave torn reads.
struct PersistState {
    file: File,
    /// Bytes consumed: we have read or written every byte before this
    /// position. The watcher skips events at offsets `< applied_offset`
    /// to avoid re-applying its own (and the daemon's own) writes.
    applied_offset: u64,
    /// Inode of the currently-open `file`. A change here means someone
    /// replaced the log via `mv` (or similar) — we reopen and reload.
    inode: u64,
}

pub struct DiskPersister {
    state: Arc<Mutex<PersistState>>,
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
        let meta = file.metadata()?;
        let state = PersistState {
            file,
            applied_offset: 0, // replay() will set this to file end
            inode: meta.ino(),
        };
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
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
        let mut state = self.state.lock().expect("disk persister mutex poisoned");
        let surviving = read_and_dedupe_from_top(&mut state.file, &self.path)?;
        // Position the cursor at end-of-file for subsequent appends and
        // record that offset as our high-water mark. (Append mode ignores
        // the seek for writes, but stream_position now reflects EOF.)
        let end = state.file.seek(SeekFrom::End(0))?;
        state.applied_offset = end;
        drop(state);

        let surviving_count = surviving.len();
        let applied = ctx
            .apply_event_batch(surviving)
            .map_err(|e| std::io::Error::other(format!("apply_event_batch: {e}")))?;

        log::info!(
            "[disk-persister] replayed event log {} ({} survived dedup, {} applied)",
            self.path.display(),
            surviving_count,
            applied,
        );

        Ok(applied)
    }

    /// Spawn a filesystem watcher that tails the event log for external
    /// appends, truncations, and replacements. Caller must keep the
    /// returned `DiskWatcher` alive for the lifetime of the daemon — it
    /// owns the underlying notify debouncer thread.
    ///
    /// Self-write suppression: every persist/replay updates
    /// `applied_offset`. The watcher only reads bytes past that mark, so
    /// the daemon's own appends are skipped (file size doesn't grow
    /// beyond the new offset before the watcher tick fires).
    ///
    /// Truncation / replacement: detected by `current_len <
    /// applied_offset` or by an inode change. In either case the
    /// persister reopens its file handle (so subsequent appends land in
    /// the new file, not an unlinked one) and re-runs the
    /// dedupe-from-top loader. Pre-existing entities not in the new file
    /// are not DEL'd — hot-reload is a recovery affordance, not a full
    /// state-replacement primitive.
    pub fn start_watcher(
        self: &Arc<Self>,
        ctx: CellServerCtx,
    ) -> notify_debouncer_mini::notify::Result<DiskWatcher> {
        let me = Arc::clone(self);
        let mut debouncer: Debouncer<RecommendedWatcher> = new_debouncer(
            Duration::from_millis(150),
            move |result: DebounceEventResult| match result {
                Ok(_events) => {
                    if let Err(e) = me.tail_apply(&ctx) {
                        log::warn!("[watcher] tail apply failed: {e}");
                    }
                }
                Err(errors) => {
                    log::warn!("[watcher] notify error: {errors:?}");
                }
            },
        )?;
        debouncer
            .watcher()
            .watch(&self.path, RecursiveMode::NonRecursive)?;
        log::info!("[watcher] tailing event log {}", self.path.display());
        Ok(DiskWatcher {
            _debouncer: debouncer,
        })
    }

    /// Read any new bytes at the tail of the log and apply them as live
    /// events. Idempotent against the daemon's own writes via the
    /// `applied_offset` watermark. Mutates `state.applied_offset` to the
    /// new file length before releasing the lock so any cascading
    /// `persist()` calls inside `apply_event_batch` extend it further
    /// rather than re-applying what we just read.
    fn tail_apply(&self, ctx: &CellServerCtx) -> std::io::Result<()> {
        // Hold the state lock across the read so persist() can't
        // interleave a partial line into our buffer.
        let mut state = self.state.lock().expect("disk persister mutex poisoned");

        let meta = std::fs::metadata(&self.path)?;
        let current_len = meta.len();
        let current_inode = meta.ino();

        let inode_changed = current_inode != state.inode;
        let truncated = current_len < state.applied_offset;
        if inode_changed || truncated {
            log::warn!(
                "[watcher] events.jsonl {} (inode {} → {}, len {} → {}); reloading from top",
                if truncated { "truncated" } else { "replaced" },
                state.inode,
                current_inode,
                state.applied_offset,
                current_len,
            );
            // Reopen the path so future appends land in the new inode.
            let new_file = OpenOptions::new()
                .read(true)
                .append(true)
                .create(true)
                .open(&self.path)?;
            state.file = new_file;
            state.inode = current_inode;
            let surviving = read_and_dedupe_from_top(&mut state.file, &self.path)?;
            let end = state.file.seek(SeekFrom::End(0))?;
            state.applied_offset = end;
            drop(state);
            apply_or_log(ctx, surviving, "full-reload");
            return Ok(());
        }

        if current_len == state.applied_offset {
            return Ok(()); // No new bytes; nothing to do.
        }

        let start = state.applied_offset;
        let span = current_len - start;
        state.file.seek(SeekFrom::Start(start))?;
        let mut buf = String::new();
        // Read exactly the bytes that were present when we observed the
        // length, not whatever may have been appended since. This keeps
        // the offset bookkeeping aligned with what we actually consumed.
        BufReader::new(&mut state.file)
            .take(span)
            .read_to_string(&mut buf)?;
        // Restore stream position to EOF (informational under append mode).
        state.file.seek(SeekFrom::End(0))?;
        state.applied_offset = current_len;
        drop(state);

        let mut events: Vec<MEvent> = Vec::new();
        for line in buf.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<MEvent>(line) {
                Ok(mut e) => {
                    let mut opts = e.options.clone().unwrap_or_default();
                    opts.prevent_persist = true;
                    e.options = Some(opts);
                    events.push(e);
                }
                Err(e) => log::warn!("[watcher] skipping malformed line: {e}"),
            }
        }

        apply_or_log(ctx, events, "tail");
        Ok(())
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

        let mut state = self.state.lock().expect("disk persister mutex poisoned");
        let write_result = state
            .file
            .write_all(serialized.as_bytes())
            .and_then(|_| state.file.write_all(b"\n"))
            .and_then(|_| state.file.sync_data());

        if let Err(e) = write_result {
            self.health.record_error(e.to_string());
            return Err(PersistError {
                entity_type: event.item_type,
                message: format!("write/fsync: {e}"),
            });
        }

        // Bump the watermark so the watcher's next tick recognizes this
        // append as our own and skips it. +1 for the trailing newline.
        state.applied_offset += serialized.len() as u64 + 1;

        self.health.record_success();
        Ok(())
    }

    fn health(&self) -> Arc<PersistHealth> {
        self.health.clone()
    }
}

/// Opaque handle returned by `start_watcher`. Owns the notify debouncer
/// thread; dropping it tears the watcher down.
pub struct DiskWatcher {
    _debouncer: Debouncer<RecommendedWatcher>,
}

/// Stream the file once, collecting per `(item_type, id)` to the latest
/// MEvent, then return the surviving SETs marked replay-only
/// (`prevent_persist` + `prevent_relationship_updates`). Used by both
/// startup `replay()` and the watcher's full-reload path.
fn read_and_dedupe_from_top(file: &mut File, path: &Path) -> std::io::Result<Vec<MEvent>> {
    file.seek(SeekFrom::Start(0))?;
    let mut latest: HashMap<(String, String), MEvent> = HashMap::new();
    let mut total = 0usize;
    let mut malformed = 0usize;

    let reader = BufReader::new(&mut *file);
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
                log::warn!(
                    "[disk-persister] skipping malformed line in {}: {e}",
                    path.display()
                );
            }
        }
    }

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

    log::debug!(
        "[disk-persister] read {} ({} total, {} malformed, {} survived dedup)",
        path.display(),
        total,
        malformed,
        surviving.len(),
    );

    Ok(surviving)
}

fn apply_or_log(ctx: &CellServerCtx, events: Vec<MEvent>, label: &str) {
    if events.is_empty() {
        return;
    }
    let count = events.len();
    match ctx.apply_event_batch(events) {
        Ok(applied) => {
            log::info!("[watcher] {label}: applied {applied}/{count} hot-reloaded event(s)")
        }
        Err(e) => log::warn!("[watcher] {label}: apply_event_batch failed: {e}"),
    }
}
