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
//!
//! With the `engine-store` feature, a store that carries engine tables keeps
//! `sync_log` there (`db::engine::sync_log`, ADR-0023 phase 4e). `sync_flags`
//! and `sync_sends` move with the outbox, when the tables hold the memories
//! core (`db::engine::outbox`).

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineTables};
use super::{Result, Store};
#[cfg(feature = "engine-store")]
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};

/// One remote's `sync_log` liveness stamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLog {
    pub remote_id: String,
    pub last_attempt_at: String,
    pub last_push_at: String,
    pub last_pull_at: String,
}

/// The epoch every `sync_log` timestamp column defaults to.
const EPOCH: &str = "1970-01-01T00:00:00+00:00";

/// One whole `sync_log` row: a remote's pull cursors and liveness stamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncLogRow {
    pub remote_id: String,
    pub last_pull: String,
    pub last_push: String,
    pub last_pull_id: String,
    pub last_attempt_at: String,
    pub last_push_at: String,
    pub last_pull_at: String,
    pub last_pull_seq: i64,
}

impl SyncLogRow {
    /// A row for `remote_id` holding the schema's defaults: every timestamp
    /// the epoch, no keyset id, and the `hub_seq` cursor unknown (-1).
    pub fn new(remote_id: &str) -> Self {
        Self {
            remote_id: remote_id.to_string(),
            last_pull: EPOCH.to_string(),
            last_push: EPOCH.to_string(),
            last_pull_id: String::new(),
            last_attempt_at: EPOCH.to_string(),
            last_push_at: EPOCH.to_string(),
            last_pull_at: EPOCH.to_string(),
            last_pull_seq: -1,
        }
    }
}

/// The sync bookkeeping tables, over one store.
pub struct SyncState<'c> {
    conn: &'c Connection,
    #[cfg(feature = "engine-store")]
    engine: Option<&'c Mutex<EngineTables>>,
    /// The tables again when they hold the memories core, which keeps
    /// `sync_flags` and `sync_sends`.
    #[cfg(feature = "engine-store")]
    core: Option<&'c Mutex<EngineTables>>,
}

impl<'c> SyncState<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            #[cfg(feature = "engine-store")]
            engine: store.engine(),
            #[cfg(feature = "engine-store")]
            core: store.core(),
        }
    }

    /// `remote_id`'s whole `sync_log` row, if it has one.
    pub fn remote_row(&self, remote_id: &str) -> Result<Option<SyncLogRow>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::sync_log::row(&engine.lock(), remote_id));
        }
        Ok(self
            .conn
            .query_row(
                "SELECT remote_id, last_pull, last_push, last_pull_id, last_attempt_at,
                        last_push_at, last_pull_at, last_pull_seq
                   FROM sync_log WHERE remote_id = ?",
                params![remote_id],
                |r| {
                    Ok(SyncLogRow {
                        remote_id: r.get(0)?,
                        last_pull: r.get(1)?,
                        last_push: r.get(2)?,
                        last_pull_id: r.get(3)?,
                        last_attempt_at: r.get(4)?,
                        last_push_at: r.get(5)?,
                        last_pull_at: r.get(6)?,
                        last_pull_seq: r.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    /// Store `row` whole, replacing any row for its remote: for copying a
    /// store, and for tests that need a state no sync cycle produces.
    pub fn put_remote_row(&self, row: &SyncLogRow) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::sync_log::put(&mut engine.lock(), row);
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO sync_log
                 (remote_id, last_pull, last_push, last_pull_id, last_attempt_at,
                  last_push_at, last_pull_at, last_pull_seq)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                row.remote_id,
                row.last_pull,
                row.last_push,
                row.last_pull_id,
                row.last_attempt_at,
                row.last_push_at,
                row.last_pull_at,
                row.last_pull_seq
            ],
        )?;
        Ok(())
    }

    /// `remote_id`'s keyset pull cursor `(last_pull, last_pull_id)`, or `None`
    /// if the remote has no `sync_log` row.
    pub fn pull_cursor(&self, remote_id: &str) -> Result<Option<(String, String)>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::sync_log::row(&engine.lock(), remote_id)
                .map(|r| (r.last_pull, r.last_pull_id)));
        }
        Ok(self
            .conn
            .query_row(
                "SELECT last_pull, last_pull_id FROM sync_log WHERE remote_id = ?",
                params![remote_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// Store `remote_id`'s keyset pull cursor.
    pub fn set_pull_cursor(&self, remote_id: &str, since: &str, since_id: &str) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::sync_log::upsert(&mut engine.lock(), remote_id, |r| {
                r.last_pull = since.to_string();
                r.last_pull_id = since_id.to_string();
            });
        }
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
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::sync_log::row(&engine.lock(), remote_id).map(|r| r.last_pull_seq));
        }
        Ok(self
            .conn
            .query_row(
                "SELECT last_pull_seq FROM sync_log WHERE remote_id = ?",
                params![remote_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Store `remote_id`'s `hub_seq` pull cursor.
    pub fn set_seq_cursor(&self, remote_id: &str, seq: i64) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::sync_log::upsert(&mut engine.lock(), remote_id, |r| {
                r.last_pull_seq = seq
            });
        }
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
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::sync_log::update(&mut engine.lock(), remote_id, |r| {
                r.last_pull = since.to_string();
                r.last_pull_id = String::new();
                r.last_pull_seq = seq;
            });
        }
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
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::sync_log::upsert(&mut engine.lock(), remote_id, |r| {
                r.last_attempt_at = at.to_string();
                r.last_push_at = at.to_string();
            });
        }
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
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::sync_log::upsert(&mut engine.lock(), remote_id, |r| {
                r.last_attempt_at = at.to_string();
                r.last_pull_at = at.to_string();
            });
        }
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
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::sync_log::row(&engine.lock(), remote_id).map(|r| r.last_pull_at));
        }
        Ok(self
            .conn
            .query_row(
                "SELECT last_pull_at FROM sync_log WHERE remote_id = ?",
                params![remote_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Every remote's liveness stamps, ordered by `remote_id`.
    pub fn remotes(&self) -> Result<Vec<RemoteLog>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::sync_log::remotes(&engine.lock()));
        }
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
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// The `sync_flags` value under `key`, if set.
    pub fn flag(&self, key: &str) -> Result<Option<String>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::core_ref(&core.lock()).map(|core| engine::outbox::flag(core, key));
        }
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM sync_flags WHERE key = ?",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Set the `sync_flags` value under `key`.
    pub fn set_flag(&self, key: &str, value: &str) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::outbox::set_flag(&mut core.lock(), key, value);
        }
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
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::outbox::record_sends(&mut core.lock(), remote_id, outbox_ids, at);
        }
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
    use crate::db::{on_each_backend, Database};

    #[test]
    fn cursor_writes_touch_only_their_own_columns() {
        on_each_backend(cursor_writes_touch_only_their_own_columns_on);
    }

    #[test]
    fn a_whole_row_round_trips_and_new_rows_take_the_defaults() {
        on_each_backend(|db| {
            let store = db.store();
            let state = SyncState::new(&store);
            state.set_seq_cursor("hub", 3).unwrap();
            assert_eq!(
                state.remote_row("hub").unwrap(),
                Some(SyncLogRow {
                    last_pull_seq: 3,
                    ..SyncLogRow::new("hub")
                }),
                "an upsert on a new remote starts from the schema's defaults"
            );

            let row = SyncLogRow {
                remote_id: "peer".into(),
                last_pull: "a".into(),
                last_push: "b".into(),
                last_pull_id: "c".into(),
                last_attempt_at: "d".into(),
                last_push_at: "e".into(),
                last_pull_at: "f".into(),
                last_pull_seq: 9,
            };
            state.put_remote_row(&row).unwrap();
            assert_eq!(state.remote_row("peer").unwrap(), Some(row));
            assert_eq!(state.remote_row("nowhere").unwrap(), None);
        });
    }

    fn cursor_writes_touch_only_their_own_columns_on(db: &Database) {
        let store = db.store();
        let state = SyncState::new(&store);
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
    fn flags_overwrite_on_each_backend() {
        crate::db::on_each_backend(|db| {
            let store = db.store();
            let state = SyncState::new(&store);
            assert_eq!(state.flag("k").unwrap(), None);
            state.set_flag("k", "1").unwrap();
            state.set_flag("k", "2").unwrap();
            assert_eq!(state.flag("k").unwrap().as_deref(), Some("2"));
        });
    }

    #[cfg(feature = "engine-store")]
    #[test]
    fn sends_replace_on_the_engine() {
        let db = Database::open_in_memory_on_engine().unwrap();
        let store = db.store();
        let state = SyncState::new(&store);
        state.record_sends("hub", &[1, 2], "t1").unwrap();
        state.record_sends("hub", &[2], "t2").unwrap();
        let tables = store.engine().unwrap().lock();
        assert_eq!(
            engine::outbox::sent(&tables),
            vec![(1, "t1".to_string()), (2, "t2".to_string())]
        );
    }

    #[test]
    fn flags_overwrite_and_sends_replace() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let state = SyncState::new(&store);
        state.set_flag("k", "2").unwrap();
        assert_eq!(state.flag("k").unwrap().as_deref(), Some("2"));

        state.record_sends("hub", &[1, 2], "t1").unwrap();
        state.record_sends("hub", &[2], "t2").unwrap();
        let sent: Vec<(i64, String)> = crate::testing::sends(&store)
            .unwrap()
            .into_iter()
            .map(|(_, outbox_id, sent_at)| (outbox_id, sent_at))
            .collect();
        assert_eq!(sent, vec![(1, "t1".into()), (2, "t2".into())]);
    }
}
