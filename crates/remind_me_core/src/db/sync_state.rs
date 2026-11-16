//! Storage for sync bookkeeping: per-remote cursors and liveness stamps in
//! `sync_log`, key-value flags in `sync_flags` and which outbox rows went to
//! which remote in `sync_sends`, behind the [`SyncStateStore`] port
//! (ADR-0035). The engine implements it in `db::engine::sync_log`, over its
//! own log table and the outbox's flag and send tables.
//!
//! Every read or write that touches only those tables goes through here
//! (ADR-0022). Reads that also take `sync_outbox` (fetching a push batch,
//! counting pending rows, clearing and pruning) belong to
//! [`crate::db::outbox`].
//!
//! Policy stays in [`crate::sync`]: what a missing cursor means, what the
//! epoch default means, and when a stamp is best-effort.

use super::engine::EngineLock;
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

/// The port for the sync bookkeeping tables (ADR-0035). [`conformance`]
/// holds the behaviour every implementation must show.
pub trait SyncStateStore {
    /// `remote_id`'s whole `sync_log` row, if it has one.
    fn remote_row(&self, remote_id: &str) -> Result<Option<SyncLogRow>>;

    /// Store `row` whole, replacing any row for its remote: for copying a
    /// store, and for tests that need a state no sync cycle produces.
    fn put_remote_row(&mut self, row: &SyncLogRow) -> Result<()>;

    /// `remote_id`'s keyset pull cursor `(last_pull, last_pull_id)`, or `None`
    /// if the remote has no `sync_log` row.
    fn pull_cursor(&self, remote_id: &str) -> Result<Option<(String, String)>>;

    /// Store `remote_id`'s keyset pull cursor. A remote with no row gets one
    /// holding [`SyncLogRow::new`]'s defaults first.
    fn set_pull_cursor(&mut self, remote_id: &str, since: &str, since_id: &str) -> Result<()>;

    /// `remote_id`'s `hub_seq` pull cursor, or `None` if the remote has no
    /// `sync_log` row.
    fn seq_cursor(&self, remote_id: &str) -> Result<Option<i64>>;

    /// Store `remote_id`'s `hub_seq` pull cursor, creating its row as
    /// [`SyncStateStore::set_pull_cursor`] does.
    fn set_seq_cursor(&mut self, remote_id: &str, seq: i64) -> Result<()>;

    /// Reset `remote_id`'s pull cursors to `since` (with an empty id) and
    /// `seq`, leaving its liveness stamps alone. `false` if it has no row.
    fn reset_pull_cursors(&mut self, remote_id: &str, since: &str, seq: i64) -> Result<bool>;

    /// Stamp a successful push to `remote_id` at `at`: `last_attempt_at` and
    /// `last_push_at`.
    fn stamp_push(&mut self, remote_id: &str, at: &str) -> Result<()>;

    /// Stamp a successful pull from `remote_id` at `at`: `last_attempt_at`
    /// and `last_pull_at`.
    fn stamp_pull(&mut self, remote_id: &str, at: &str) -> Result<()>;

    /// `remote_id`'s `last_pull_at`, or `None` if it has no row.
    fn last_pull_at(&self, remote_id: &str) -> Result<Option<String>>;

    /// Every remote's liveness stamps, ordered by `remote_id`.
    fn remotes(&self) -> Result<Vec<RemoteLog>>;

    /// The `sync_flags` value under `key`, if set.
    fn flag(&self, key: &str) -> Result<Option<String>>;

    /// Set the `sync_flags` value under `key`. `sync_enabled` set to `"1"`
    /// turns on outbox queueing for local writes.
    fn set_flag(&mut self, key: &str, value: &str) -> Result<()>;

    /// Record that `outbox_ids` were sent to `remote_id` at `at`. Sending one
    /// again replaces its earlier record.
    fn record_sends(&mut self, remote_id: &str, outbox_ids: &[i64], at: &str) -> Result<()>;
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

    /// See [`SyncStateStore::remote_row`].
    pub fn remote_row(&self, remote_id: &str) -> Result<Option<SyncLogRow>> {
        SyncStateStore::remote_row(&*self.engine.lock(), remote_id)
    }

    /// See [`SyncStateStore::put_remote_row`].
    pub fn put_remote_row(&self, row: &SyncLogRow) -> Result<()> {
        SyncStateStore::put_remote_row(&mut *self.engine.lock(), row)
    }

    /// See [`SyncStateStore::pull_cursor`].
    pub fn pull_cursor(&self, remote_id: &str) -> Result<Option<(String, String)>> {
        SyncStateStore::pull_cursor(&*self.engine.lock(), remote_id)
    }

    /// See [`SyncStateStore::set_pull_cursor`].
    pub fn set_pull_cursor(&self, remote_id: &str, since: &str, since_id: &str) -> Result<()> {
        SyncStateStore::set_pull_cursor(&mut *self.engine.lock(), remote_id, since, since_id)
    }

    /// See [`SyncStateStore::seq_cursor`].
    pub fn seq_cursor(&self, remote_id: &str) -> Result<Option<i64>> {
        SyncStateStore::seq_cursor(&*self.engine.lock(), remote_id)
    }

    /// See [`SyncStateStore::set_seq_cursor`].
    pub fn set_seq_cursor(&self, remote_id: &str, seq: i64) -> Result<()> {
        SyncStateStore::set_seq_cursor(&mut *self.engine.lock(), remote_id, seq)
    }

    /// See [`SyncStateStore::reset_pull_cursors`].
    pub fn reset_pull_cursors(&self, remote_id: &str, since: &str, seq: i64) -> Result<bool> {
        SyncStateStore::reset_pull_cursors(&mut *self.engine.lock(), remote_id, since, seq)
    }

    /// See [`SyncStateStore::stamp_push`].
    pub fn stamp_push(&self, remote_id: &str, at: &str) -> Result<()> {
        SyncStateStore::stamp_push(&mut *self.engine.lock(), remote_id, at)
    }

    /// See [`SyncStateStore::stamp_pull`].
    pub fn stamp_pull(&self, remote_id: &str, at: &str) -> Result<()> {
        SyncStateStore::stamp_pull(&mut *self.engine.lock(), remote_id, at)
    }

    /// See [`SyncStateStore::last_pull_at`].
    pub fn last_pull_at(&self, remote_id: &str) -> Result<Option<String>> {
        SyncStateStore::last_pull_at(&*self.engine.lock(), remote_id)
    }

    /// See [`SyncStateStore::remotes`].
    pub fn remotes(&self) -> Result<Vec<RemoteLog>> {
        SyncStateStore::remotes(&*self.engine.lock())
    }

    /// See [`SyncStateStore::flag`].
    pub fn flag(&self, key: &str) -> Result<Option<String>> {
        SyncStateStore::flag(&*self.engine.lock(), key)
    }

    /// See [`SyncStateStore::set_flag`].
    pub fn set_flag(&self, key: &str, value: &str) -> Result<()> {
        SyncStateStore::set_flag(&mut *self.engine.lock(), key, value)
    }

    /// See [`SyncStateStore::record_sends`].
    pub fn record_sends(&self, remote_id: &str, outbox_ids: &[i64], at: &str) -> Result<()> {
        SyncStateStore::record_sends(&mut *self.engine.lock(), remote_id, outbox_ids, at)
    }
}

/// The behaviour every [`SyncStateStore`] must show, run against each
/// backend by [`sync_state_conformance`] (ADR-0035). Which outbox rows a
/// send marks is checked with the outbox, in its own suite.
#[cfg(test)]
pub(crate) mod conformance {
    use super::{RemoteLog, SyncLogRow, SyncStateStore};

    pub(crate) fn a_whole_row_round_trips_and_new_rows_take_the_defaults(
        store: &mut dyn SyncStateStore,
    ) {
        store.set_seq_cursor("hub", 3).unwrap();
        assert_eq!(
            store.remote_row("hub").unwrap(),
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
        store.put_remote_row(&row).unwrap();
        assert_eq!(store.remote_row("peer").unwrap(), Some(row));
        assert_eq!(store.remote_row("nowhere").unwrap(), None);
    }

    pub(crate) fn cursor_writes_touch_only_their_own_columns(store: &mut dyn SyncStateStore) {
        assert_eq!(store.pull_cursor("hub").unwrap(), None);
        assert_eq!(store.seq_cursor("hub").unwrap(), None);

        store
            .stamp_pull("hub", "2026-09-25T01:00:00+00:00")
            .unwrap();
        store.set_seq_cursor("hub", 7).unwrap();
        store
            .set_pull_cursor("hub", "2026-09-25T00:00:00+00:00", "m9")
            .unwrap();
        store
            .stamp_push("hub", "2026-09-25T02:00:00+00:00")
            .unwrap();

        assert_eq!(store.seq_cursor("hub").unwrap(), Some(7));
        assert_eq!(
            store.pull_cursor("hub").unwrap(),
            Some(("2026-09-25T00:00:00+00:00".into(), "m9".into()))
        );
        assert_eq!(
            store.remotes().unwrap(),
            vec![RemoteLog {
                remote_id: "hub".into(),
                last_attempt_at: "2026-09-25T02:00:00+00:00".into(),
                last_push_at: "2026-09-25T02:00:00+00:00".into(),
                last_pull_at: "2026-09-25T01:00:00+00:00".into(),
            }]
        );

        assert!(store
            .reset_pull_cursors("hub", "1970-01-01T00:00:00+00:00", -1)
            .unwrap());
        assert_eq!(store.seq_cursor("hub").unwrap(), Some(-1));
        assert_eq!(
            store.last_pull_at("hub").unwrap().as_deref(),
            Some("2026-09-25T01:00:00+00:00"),
            "a reset keeps the liveness stamps"
        );
        assert!(!store.reset_pull_cursors("nowhere", "x", -1).unwrap());
    }

    pub(crate) fn flags_overwrite(store: &mut dyn SyncStateStore) {
        assert_eq!(store.flag("k").unwrap(), None);
        store.set_flag("k", "1").unwrap();
        store.set_flag("k", "2").unwrap();
        assert_eq!(store.flag("k").unwrap().as_deref(), Some("2"));
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`.
#[cfg(test)]
macro_rules! sync_state_conformance {
    ($make:expr) => {
        use $crate::db::sync_state::conformance;

        #[test]
        fn a_whole_row_round_trips_and_new_rows_take_the_defaults() {
            conformance::a_whole_row_round_trips_and_new_rows_take_the_defaults(&mut $make);
        }
        #[test]
        fn cursor_writes_touch_only_their_own_columns() {
            conformance::cursor_writes_touch_only_their_own_columns(&mut $make);
        }
        #[test]
        fn flags_overwrite() {
            conformance::flags_overwrite(&mut $make);
        }
    };
}
#[cfg(test)]
pub(crate) use sync_state_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::engine;
    use crate::db::Database;

    /// The facade reaches the engine through the port, and a send sent again
    /// replaces its record in the engine's send table.
    #[test]
    fn sends_replace_and_the_facade_reads_through_the_port() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let state = SyncState::new(&store);
        state.set_flag("k", "1").unwrap();
        assert_eq!(state.flag("k").unwrap().as_deref(), Some("1"));
        state.stamp_push("hub", "t0").unwrap();
        assert_eq!(state.remotes().unwrap().len(), 1);

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
