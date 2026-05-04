//! SQLite message store.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path).context("opening sqlite db")?;
        Self::init(&conn)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory sqlite db")?;
        Self::init(&conn)?;
        Ok(Self { conn })
    }

    fn init(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS messages (
              id           INTEGER PRIMARY KEY,
              from_session TEXT NOT NULL,
              from_nick    TEXT NOT NULL,
              to_session   TEXT NOT NULL,
              to_nick      TEXT NOT NULL,
              body         TEXT NOT NULL,
              sent_at      INTEGER NOT NULL,
              read_at      INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_messages_to ON messages(to_session, read_at);
            CREATE INDEX IF NOT EXISTS idx_messages_sent_at ON messages(sent_at);
            "#,
        )?;
        Ok(())
    }

    /// Insert and return the new id.
    pub fn insert_message(
        &self,
        from_session: &str,
        from_nick: &str,
        to_session: &str,
        to_nick: &str,
        body: &str,
        sent_at_ms: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO messages (from_session, from_nick, to_session, to_nick, body, sent_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![from_session, from_nick, to_session, to_nick, body, sent_at_ms],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Unread messages addressed to `to_session`, oldest first.
    pub fn unread_for(&self, to_session: &str) -> Result<Vec<StoredMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, from_session, from_nick, body, sent_at \
             FROM messages WHERE to_session = ?1 AND read_at IS NULL \
             ORDER BY sent_at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![to_session], row_to_stored)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn mark_read(&self, ids: &[i64], now_ms: i64) -> Result<usize> {
        if ids.is_empty() { return Ok(0); }
        let placeholders = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "UPDATE messages SET read_at = ? WHERE id IN ({}) AND read_at IS NULL",
            placeholders
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now_ms)];
        for id in ids { params_vec.push(Box::new(*id)); }
        let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
        let n = stmt.execute(rusqlite::params_from_iter(refs))?;
        Ok(n)
    }

    /// Recent messages involving `me` (sent or received), newest first, up to `limit`.
    pub fn recent_for(&self, me: &str, limit: u32) -> Result<Vec<RecentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, from_session, from_nick, to_session, to_nick, body, sent_at \
             FROM messages WHERE from_session = ?1 OR to_session = ?1 \
             ORDER BY sent_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![me, limit], |row| {
            Ok(RecentRow {
                id: row.get(0)?,
                from_session: row.get(1)?,
                from_nick: row.get(2)?,
                to_session: row.get(3)?,
                to_nick: row.get(4)?,
                body: row.get(5)?,
                sent_at: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn prune_older_than(&self, cutoff_ms: i64) -> Result<usize> {
        let n = self.conn.execute("DELETE FROM messages WHERE sent_at < ?1", params![cutoff_ms])?;
        Ok(n)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    pub id: i64,
    pub from_session: String,
    pub from_nick: String,
    pub body: String,
    pub sent_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentRow {
    pub id: i64,
    pub from_session: String,
    pub from_nick: String,
    pub to_session: String,
    pub to_nick: String,
    pub body: String,
    pub sent_at: i64,
}

fn row_to_stored(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    Ok(StoredMessage {
        id: row.get(0)?,
        from_session: row.get(1)?,
        from_nick: row.get(2)?,
        body: row.get(3)?,
        sent_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_unread() {
        let s = Store::open_in_memory().unwrap();
        s.insert_message("s-a", "a", "s-b", "b", "hi", 100).unwrap();
        let unread = s.unread_for("s-b").unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].body, "hi");
        assert_eq!(unread[0].from_nick, "a");
    }

    #[test]
    fn unread_excludes_read() {
        let s = Store::open_in_memory().unwrap();
        let id = s.insert_message("s-a", "a", "s-b", "b", "hi", 100).unwrap();
        s.mark_read(&[id], 200).unwrap();
        assert!(s.unread_for("s-b").unwrap().is_empty());
    }

    #[test]
    fn recent_includes_both_directions_newest_first() {
        let s = Store::open_in_memory().unwrap();
        s.insert_message("s-a", "a", "s-b", "b", "1st", 100).unwrap();
        s.insert_message("s-b", "b", "s-a", "a", "reply", 200).unwrap();
        let rows = s.recent_for("s-a", 50).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].body, "reply");
        assert_eq!(rows[1].body, "1st");
    }

    #[test]
    fn prune_drops_old_rows() {
        let s = Store::open_in_memory().unwrap();
        s.insert_message("s-a", "a", "s-b", "b", "old", 50).unwrap();
        s.insert_message("s-a", "a", "s-b", "b", "new", 500).unwrap();
        let n = s.prune_older_than(100).unwrap();
        assert_eq!(n, 1);
        assert_eq!(s.unread_for("s-b").unwrap().len(), 1);
    }
}
