//! Storage for sync bookkeeping: per-remote cursors and liveness stamps in
//! `sync_log`, key-value flags in `sync_flags`, and which outbox rows went to
//! which remote in `sync_sends`.
//!
//! Every statement that only reads or writes those tables lives here
//! (ADR-0022). Statements that also read `sync_outbox` (fetching a push batch,
//! counting pending rows, clearing and pruning) belong to [`crate::db::outbox`].
//!
//! Policy stays in [`crate::sync`] too: what a missing cursor means, what the
//! epoch default means, and when a stamp is best-effort.

use rusqlite::{params, Connection, OptionalExtension, Result};

/// One remote's `sync_log` liveness stamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLog {
    pub remote_id: String,
    pub last_attempt_at: String,
    pub last_push_at: String,
    pub last_pull_at: String,
}

/// The sync bookkeeping tables, over one connection.
pub struct SyncState<'c> {
    conn: &'c Connection,
}

impl<'c> SyncState<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// `remote_id`'s keyset pull cursor `(last_pull, last_pull_id)`, or `None`
    /// if the remote has no `sync_log` row.
    pub fn pull_cursor(&self, remote_id: &str) -> Result<Option<(String, String)>> {
        self.conn
            .query_row(
                "SELECT last_pull, last_pull_id FROM sync_log WHERE remote_id = ?",
                params![remote_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    }

    /// Store `remote_id`'s keyset pull cursor.
    pub fn set_pull_cursor(&self, remote_id: &str, since: &str, since_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_log (remote_id, last_pull, last_pull_id) VALUES (?, ?, ?)
             ON CONFLICT(remote_id) DO UPDATE SET
                 last_pull = excluded.last_pull,
                 last_pull_id = excluded.last_pull_id",
            params![remote_id, since, since_id],
        )?;
        Ok(())
    }

    /// `remote_id`'s `hub_seq` pull cursor, or `None` if the remote has no
    /// `sync_log` row.
    pub fn seq_cursor(&self, remote_id: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT last_pull_seq FROM sync_log WHERE remote_id = ?",
                params![remote_id],
                |row| row.get(0),
            )
            .optional()
    }

    /// Store `remote_id`'s `hub_seq` pull cursor.
    pub fn set_seq_cursor(&self, remote_id: &str, seq: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_log (remote_id, last_pull_seq) VALUES (?, ?)
             ON CONFLICT(remote_id) DO UPDATE SET last_pull_seq = excluded.last_pull_seq",
            params![remote_id, seq],
        )?;
        Ok(())
    }

    /// Reset `remote_id`'s pull cursors to `since` (with an empty id) and
    /// `seq`, leaving its liveness stamps alone. `false` if it has no row.
    pub fn reset_pull_cursors(&self, remote_id: &str, since: &str, seq: i64) -> Result<bool> {
        let affected = self.conn.execute(
            "UPDATE sync_log
                SET last_pull = ?, last_pull_id = '', last_pull_seq = ?
              WHERE remote_id = ?",
            params![since, seq, remote_id],
        )?;
        Ok(affected > 0)
    }

    /// Stamp a successful push to `remote_id` at `at`: `last_attempt_at` and
    /// `last_push_at`.
    pub fn stamp_push(&self, remote_id: &str, at: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_log (remote_id, last_attempt_at, last_push_at) VALUES (?, ?, ?)
             ON CONFLICT(remote_id) DO UPDATE SET
                 last_attempt_at = excluded.last_attempt_at,
                 last_push_at = excluded.last_push_at",
            params![remote_id, at, at],
        )?;
        Ok(())
    }

    /// Stamp a successful pull from `remote_id` at `at`: `last_attempt_at`
    /// and `last_pull_at`.
    pub fn stamp_pull(&self, remote_id: &str, at: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_log (remote_id, last_attempt_at, last_pull_at) VALUES (?, ?, ?)
             ON CONFLICT(remote_id) DO UPDATE SET
                 last_attempt_at = excluded.last_attempt_at,
                 last_pull_at = excluded.last_pull_at",
            params![remote_id, at, at],
        )?;
        Ok(())
    }

    /// `remote_id`'s `last_pull_at`, or `None` if it has no row.
    pub fn last_pull_at(&self, remote_id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT last_pull_at FROM sync_log WHERE remote_id = ?",
                params![remote_id],
                |r| r.get(0),
            )
            .optional()
    }

    /// Every remote's liveness stamps, ordered by `remote_id`.
    pub fn remotes(&self) -> Result<Vec<RemoteLog>> {
        let mut stmt = self.conn.prepare(
            "SELECT remote_id, last_attempt_at, last_push_at, last_pull_at
               FROM sync_log ORDER BY remote_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(RemoteLog {
                    remote_id: r.get(0)?,
                    last_attempt_at: r.get(1)?,
                    last_push_at: r.get(2)?,
                    last_pull_at: r.get(3)?,
                })
            })?
            .collect();
        rows
    }

    /// The `sync_flags` value under `key`, if set.
    pub fn flag(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM sync_flags WHERE key = ?",
                params![key],
                |r| r.get(0),
            )
            .optional()
    }

    /// Set the `sync_flags` value under `key`.
    pub fn set_flag(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_flags (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Record that `outbox_ids` were sent to `remote_id` at `at`. Sending one
    /// again replaces its earlier record.
    pub fn record_sends(&self, remote_id: &str, outbox_ids: &[i64], at: &str) -> Result<()> {
        for id in outbox_ids {
            self.conn.execute(
                "INSERT OR REPLACE INTO sync_sends (remote_id, outbox_id, sent_at) VALUES (?, ?, ?)",
                params![remote_id, id, at],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn cursor_writes_touch_only_their_own_columns() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let state = SyncState::new(&conn);
        assert_eq!(state.pull_cursor("hub").unwrap(), None);
        assert_eq!(state.seq_cursor("hub").unwrap(), None);

        state
            .stamp_pull("hub", "2026-09-25T01:00:00+00:00")
            .unwrap();
        state.set_seq_cursor("hub", 7).unwrap();
        state
            .set_pull_cursor("hub", "2026-09-25T00:00:00+00:00", "m9")
            .unwrap();
        state
            .stamp_push("hub", "2026-09-25T02:00:00+00:00")
            .unwrap();

        assert_eq!(state.seq_cursor("hub").unwrap(), Some(7));
        assert_eq!(
            state.pull_cursor("hub").unwrap(),
            Some(("2026-09-25T00:00:00+00:00".into(), "m9".into()))
        );
        assert_eq!(
            state.remotes().unwrap(),
            vec![RemoteLog {
                remote_id: "hub".into(),
                last_attempt_at: "2026-09-25T02:00:00+00:00".into(),
                last_push_at: "2026-09-25T02:00:00+00:00".into(),
                last_pull_at: "2026-09-25T01:00:00+00:00".into(),
            }]
        );

        assert!(state
            .reset_pull_cursors("hub", "1970-01-01T00:00:00+00:00", -1)
            .unwrap());
        assert_eq!(state.seq_cursor("hub").unwrap(), Some(-1));
        assert_eq!(
            state.last_pull_at("hub").unwrap().as_deref(),
            Some("2026-09-25T01:00:00+00:00"),
            "a reset keeps the liveness stamps"
        );
        assert!(!state.reset_pull_cursors("nowhere", "x", -1).unwrap());
    }

    #[test]
    fn flags_overwrite_and_sends_replace() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let state = SyncState::new(&conn);
        assert_eq!(state.flag("k").unwrap(), None);
        state.set_flag("k", "1").unwrap();
        state.set_flag("k", "2").unwrap();
        assert_eq!(state.flag("k").unwrap().as_deref(), Some("2"));

        state.record_sends("hub", &[1, 2], "t1").unwrap();
        state.record_sends("hub", &[2], "t2").unwrap();
        let sent: Vec<(i64, String)> = conn
            .prepare("SELECT outbox_id, sent_at FROM sync_sends ORDER BY outbox_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(sent, vec![(1, "t1".into()), (2, "t2".into())]);
    }
}
