//! Storage for sync bookkeeping: per-remote cursors and liveness stamps in
//! `sync_log` (`db::engine::sync_log`), key-value flags in `sync_flags` and
//! which outbox rows went to which remote in `sync_sends` (both with the
//! outbox, `db::engine::outbox`).
//!
//! Every read or write that touches only those tables goes through here
//! (ADR-0022). Reads that also take `sync_outbox` (fetching a push batch,
//! counting pending rows, clearing and pruning) belong to
//! [`crate::db::outbox`].
//!
//! Policy stays in [`crate::sync`]: what a missing cursor means, what the
//! epoch default means, and when a stamp is best-effort.

use super::engine::{self, EngineLock};
use super::{Result, Store};

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
    /// A row for `remote_id` holding the defaults: every timestamp the
    /// epoch, no keyset id, and the `hub_seq` cursor unknown (-1).
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

/// The sync bookkeeping tables, on the engine.
pub struct SyncState<'c> {
    engine: &'c EngineLock,
}

impl<'c> SyncState<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            engine: store.engine(),
        }
    }

    /// `remote_id`'s whole `sync_log` row, if it has one.
    pub fn remote_row(&self, remote_id: &str) -> Result<Option<SyncLogRow>> {
        Ok(engine::sync_log::row(&self.engine.lock(), remote_id))
    }

    /// Store `row` whole, replacing any row for its remote: for copying a
    /// store, and for tests that need a state no sync cycle produces.
    pub fn put_remote_row(&self, row: &SyncLogRow) -> Result<()> {
        engine::sync_log::put(&mut self.engine.lock(), row)
    }

    /// `remote_id`'s keyset pull cursor `(last_pull, last_pull_id)`, or `None`
    /// if the remote has no `sync_log` row.
    pub fn pull_cursor(&self, remote_id: &str) -> Result<Option<(String, String)>> {
        Ok(engine::sync_log::row(&self.engine.lock(), remote_id)
            .map(|r| (r.last_pull, r.last_pull_id)))
    }

    /// Store `remote_id`'s keyset pull cursor.
    pub fn set_pull_cursor(&self, remote_id: &str, since: &str, since_id: &str) -> Result<()> {
        engine::sync_log::upsert(&mut self.engine.lock(), remote_id, |r| {
            r.last_pull = since.to_string();
            r.last_pull_id = since_id.to_string();
        })
    }

    /// `remote_id`'s `hub_seq` pull cursor, or `None` if the remote has no
    /// `sync_log` row.
    pub fn seq_cursor(&self, remote_id: &str) -> Result<Option<i64>> {
        Ok(engine::sync_log::row(&self.engine.lock(), remote_id).map(|r| r.last_pull_seq))
    }

    /// Store `remote_id`'s `hub_seq` pull cursor.
    pub fn set_seq_cursor(&self, remote_id: &str, seq: i64) -> Result<()> {
        engine::sync_log::upsert(&mut self.engine.lock(), remote_id, |r| {
            r.last_pull_seq = seq
        })
    }

    /// Reset `remote_id`'s pull cursors to `since` (with an empty id) and
    /// `seq`, leaving its liveness stamps alone. `false` if it has no row.
    pub fn reset_pull_cursors(&self, remote_id: &str, since: &str, seq: i64) -> Result<bool> {
        engine::sync_log::update(&mut self.engine.lock(), remote_id, |r| {
            r.last_pull = since.to_string();
            r.last_pull_id = String::new();
            r.last_pull_seq = seq;
        })
    }

    /// Stamp a successful push to `remote_id` at `at`: `last_attempt_at` and
    /// `last_push_at`.
    pub fn stamp_push(&self, remote_id: &str, at: &str) -> Result<()> {
        engine::sync_log::upsert(&mut self.engine.lock(), remote_id, |r| {
            r.last_attempt_at = at.to_string();
            r.last_push_at = at.to_string();
        })
    }

    /// Stamp a successful pull from `remote_id` at `at`: `last_attempt_at`
    /// and `last_pull_at`.
    pub fn stamp_pull(&self, remote_id: &str, at: &str) -> Result<()> {
        engine::sync_log::upsert(&mut self.engine.lock(), remote_id, |r| {
            r.last_attempt_at = at.to_string();
            r.last_pull_at = at.to_string();
        })
    }

    /// `remote_id`'s `last_pull_at`, or `None` if it has no row.
    pub fn last_pull_at(&self, remote_id: &str) -> Result<Option<String>> {
        Ok(engine::sync_log::row(&self.engine.lock(), remote_id).map(|r| r.last_pull_at))
    }

    /// Every remote's liveness stamps, ordered by `remote_id`.
    pub fn remotes(&self) -> Result<Vec<RemoteLog>> {
        Ok(engine::sync_log::remotes(&self.engine.lock()))
    }

    /// The `sync_flags` value under `key`, if set.
    pub fn flag(&self, key: &str) -> Result<Option<String>> {
        engine::core_ref(&self.engine.lock()).map(|core| engine::outbox::flag(core, key))
    }

    /// Set the `sync_flags` value under `key`.
    pub fn set_flag(&self, key: &str, value: &str) -> Result<()> {
        engine::outbox::set_flag(&mut self.engine.lock(), key, value)
    }

    /// Record that `outbox_ids` were sent to `remote_id` at `at`. Sending one
    /// again replaces its earlier record.
    pub fn record_sends(&self, remote_id: &str, outbox_ids: &[i64], at: &str) -> Result<()> {
        engine::outbox::record_sends(&mut self.engine.lock(), remote_id, outbox_ids, at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn a_whole_row_round_trips_and_new_rows_take_the_defaults() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let state = SyncState::new(&store);
        state.set_seq_cursor("hub", 3).unwrap();
        assert_eq!(
            state.remote_row("hub").unwrap(),
            Some(SyncLogRow {
                last_pull_seq: 3,
                ..SyncLogRow::new("hub")
            }),
            "an upsert on a new remote starts from the defaults"
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
    }

    #[test]
    fn cursor_writes_touch_only_their_own_columns() {
        let db = Database::open_in_memory().unwrap();
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
    fn flags_overwrite_and_sends_replace() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let state = SyncState::new(&store);
        assert_eq!(state.flag("k").unwrap(), None);
        state.set_flag("k", "1").unwrap();
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
        assert_eq!(
            engine::outbox::sent(&store.engine().lock()),
            vec![(1, "t1".to_string()), (2, "t2".to_string())]
        );
    }
}
