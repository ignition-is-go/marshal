//! Periodically delete messages older than 30 days.

use crate::conn::AppState;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

pub const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

pub fn spawn(app: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
        interval.tick().await; // immediate
        loop {
            run_once(&app).await;
            interval.tick().await;
        }
    })
}

pub async fn run_once(app: &AppState) {
    let cutoff = crate::conn::now_ms() - RETENTION_MS;
    let store = app.store.lock().await;
    match store.prune_older_than(cutoff) {
        Ok(0) => {}
        Ok(n) => info!(pruned = n, "pruned old messages"),
        Err(e) => warn!(error = %e, "prune failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    use crate::state::Roster;

    #[tokio::test]
    async fn run_once_drops_only_old() {
        let app = Arc::new(AppState {
            roster: Roster::new(),
            store: tokio::sync::Mutex::new(Store::open_in_memory().unwrap()),
        });
        let now = crate::conn::now_ms();
        let store = app.store.lock().await;
        store.insert_message("a", "a", "b", "b", "old", now - RETENTION_MS - 1).unwrap();
        store.insert_message("a", "a", "b", "b", "new", now).unwrap();
        drop(store);

        run_once(&app).await;

        let store = app.store.lock().await;
        let unread = store.unread_for("b").unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].body, "new");
    }
}
