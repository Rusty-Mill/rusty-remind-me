//! Storage for the sync outbox: `sync_outbox` rows (one per local change to
//! push) and their `sync_sends` markers, behind the [`OutboxStore`] port
//! (ADR-0035), which the engine's memories core implements in
//! `db::engine::outbox`.
//!
//! The rows themselves are queued by the repositories as they write, with
//! the write's [`crate::db::derived::Origin`] deciding whether. The rules
//! stay in [`crate::sync`]: when sync is enabled, the retention window,
//! batch size, and how a payload decodes.

use super::engine::EngineLock;
use super::{Result, Store};

/// One outbox row as a push reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEntry {
    pub id: i64,
    /// The `memory_id` column: the key the row was queued under, which for a
    /// link is its memory's id.
    pub key: String,
    pub payload_json: String,
}

/// The port for reading and pruning the outbox (ADR-0035). Rows are queued
/// by the other ports' local writes while `sync_enabled` is set. Send
/// markers are written through
/// [`SyncStateStore::record_sends`](crate::db::sync_state::SyncStateStore::record_sends).
/// [`conformance`] holds the behaviour every implementation must show.
pub trait OutboxStore {
    /// Up to `limit` unsent rows above `after_id` not yet sent to
    /// `remote_id`, oldest first.
    fn unsent_to(&self, remote_id: &str, after_id: i64, limit: usize) -> Result<Vec<OutboxEntry>>;

    /// How many rows have no send recorded to `remote_id`, and the oldest
    /// one's `created_at`.
    fn pending_for(&self, remote_id: &str) -> Result<(i64, Option<String>)>;

    /// How many rows the outbox holds.
    fn len(&self) -> Result<i64>;

    /// Whether the outbox holds no rows.
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// How many rows are not yet marked sent.
    fn unsent_count(&self) -> Result<i64>;

    /// Delete every sent row, every row `done_by` has taken, and every row
    /// created before `cutoff`, then the send markers left pointing at
    /// nothing. Returns how many rows went.
    fn prune(&mut self, cutoff: &str, done_by: &str) -> Result<usize>;

    /// Empty the outbox and its send markers.
    fn clear(&mut self) -> Result<()>;

    /// Queue every memory, entity and mention link as an insert, stamped
    /// now: what a node that just turned sync on owes its remotes.
    fn backfill_everything(&mut self) -> Result<()>;
}

/// The outbox, on the engine's memories core.
pub struct Outbox<'c> {
    core: &'c EngineLock,
}

impl<'c> Outbox<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    /// See [`OutboxStore::unsent_to`].
    pub fn unsent_to(
        &self,
        remote_id: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<OutboxEntry>> {
        OutboxStore::unsent_to(&*self.core.lock(), remote_id, after_id, limit)
    }

    /// See [`OutboxStore::pending_for`].
    pub fn pending_for(&self, remote_id: &str) -> Result<(i64, Option<String>)> {
        OutboxStore::pending_for(&*self.core.lock(), remote_id)
    }

    /// See [`OutboxStore::len`].
    pub fn len(&self) -> Result<i64> {
        OutboxStore::len(&*self.core.lock())
    }

    /// See [`OutboxStore::unsent_count`].
    pub fn unsent_count(&self) -> Result<i64> {
        OutboxStore::unsent_count(&*self.core.lock())
    }

    /// See [`OutboxStore::is_empty`].
    pub fn is_empty(&self) -> Result<bool> {
        OutboxStore::is_empty(&*self.core.lock())
    }

    /// See [`OutboxStore::prune`].
    pub fn prune(&self, cutoff: &str, done_by: &str) -> Result<usize> {
        OutboxStore::prune(&mut *self.core.lock(), cutoff, done_by)
    }

    /// See [`OutboxStore::clear`].
    pub fn clear(&self) -> Result<()> {
        OutboxStore::clear(&mut *self.core.lock())
    }

    /// See [`OutboxStore::backfill_everything`].
    pub fn backfill_everything(&self) -> Result<()> {
        OutboxStore::backfill_everything(&mut *self.core.lock())
    }
}

/// The behaviour every [`OutboxStore`] must show, run against each backend
/// by [`outbox_conformance`] (ADR-0035).
///
/// Rows are queued by memory and entity writes, and marked sent through the
/// sync state, so a backend under test implements [`MemoryStore`],
/// [`EntityStore`] and [`SyncStateStore`] too.
#[cfg(test)]
pub(crate) mod conformance {
    use super::OutboxStore;
    use crate::db::entities::EntityStore;
    use crate::db::memories::{MemoryStore, NewMemory};
    use crate::db::sync_state::SyncStateStore;
    use crate::entity::Entity;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    /// Sync off queues nothing; sync on queues memory and entity writes,
    /// which sends, clearing, backfill and pruning then account for.
    pub(crate) fn the_outbox_sends_and_backfill_agree<
        S: OutboxStore + SyncStateStore + MemoryStore + EntityStore,
    >(
        store: &mut S,
    ) {
        MemoryStore::insert(store, &NewMemory::new("off", "x", NOW)).unwrap();
        assert_eq!(
            store.len().unwrap(),
            0,
            "nothing is queued while sync is off"
        );

        store.set_flag("sync_enabled", "1").unwrap();
        MemoryStore::insert(store, &NewMemory::new("a", "x", NOW)).unwrap();
        MemoryStore::insert(store, &NewMemory::new("b", "y", NOW)).unwrap();
        let queued = store.unsent_to("hub", 0, 10).unwrap();
        let keys: Vec<&str> = queued.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["a", "b"]);

        store.record_sends("hub", &[queued[0].id], "t1").unwrap();
        assert_eq!(store.pending_for("hub").unwrap().0, 1);
        assert_eq!(store.pending_for("peer").unwrap().0, 2);
        assert_eq!(store.unsent_to("hub", 0, 10).unwrap().len(), 1);
        assert!(store
            .unsent_to("peer", queued[1].id, 10)
            .unwrap()
            .is_empty());
        assert_eq!(store.unsent_count().unwrap(), 2);

        // The graph queues into the same outbox.
        EntityStore::insert(
            store,
            &Entity {
                id: "ent_1".into(),
                name: "Quokka".into(),
                kind: None,
                aliases: Vec::new(),
                created_at: NOW.into(),
                updated_at: NOW.into(),
            },
            None,
        )
        .unwrap();
        assert_eq!(store.len().unwrap(), 3);

        store.clear().unwrap();
        assert_eq!(store.len().unwrap(), 0);
        assert_eq!(store.pending_for("hub").unwrap(), (0, None));

        store.backfill_everything().unwrap();
        // Memories in scan order, which the store does not promise.
        let mut keys: Vec<String> = store
            .unsent_to("hub", 0, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();
        keys.sort();
        assert_eq!(keys, ["a", "b", "ent_1", "off"]);
        assert_eq!(store.prune("9999", "hub").unwrap(), 4);
        assert_eq!(store.len().unwrap(), 0);
    }

    /// A row the remote has taken goes at a prune, however new; the rest
    /// stay until the cutoff passes them.
    pub(crate) fn prune_drops_rows_the_remote_has_taken<
        S: OutboxStore + SyncStateStore + MemoryStore,
    >(
        store: &mut S,
    ) {
        store.set_flag("sync_enabled", "1").unwrap();
        MemoryStore::insert(store, &NewMemory::new("a", "x", NOW)).unwrap();
        MemoryStore::insert(store, &NewMemory::new("b", "y", NOW)).unwrap();
        let first = store.unsent_to("hub", 0, 10).unwrap()[0].id;
        store.record_sends("hub", &[first], "t1").unwrap();

        assert_eq!(store.prune("1970-01-01", "hub").unwrap(), 1);
        let left: Vec<String> = store
            .unsent_to("peer", 0, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();
        assert_eq!(left, ["b"]);
        assert_eq!(store.pending_for("hub").unwrap().0, 1);
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`.
#[cfg(test)]
macro_rules! outbox_conformance {
    ($make:expr) => {
        use $crate::db::outbox::conformance;

        #[test]
        fn the_outbox_sends_and_backfill_agree() {
            conformance::the_outbox_sends_and_backfill_agree(&mut $make);
        }
        #[test]
        fn prune_drops_rows_the_remote_has_taken() {
            conformance::prune_drops_rows_the_remote_has_taken(&mut $make);
        }
    };
}
#[cfg(test)]
pub(crate) use outbox_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn queue(store: &Store<'_>, key: &str, created_at: &str) -> i64 {
        crate::testing::queue_outbox(store, key, "insert", "{}", created_at).unwrap()
    }

    #[test]
    fn pending_counts_rows_with_no_send_to_the_remote() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let outbox = Outbox::new(&store);
        assert!(outbox.is_empty().unwrap());
        let first = queue(&store, "a", "2026-09-01");
        queue(&store, "b", "2026-09-02");
        crate::db::sync_state::SyncState::new(&store)
            .record_sends("hub", &[first], "2026-09-03")
            .unwrap();

        assert_eq!(
            outbox.pending_for("hub").unwrap(),
            (1, Some("2026-09-02".to_string()))
        );
        assert_eq!(outbox.pending_for("peer").unwrap().0, 2);
        assert_eq!(outbox.unsent_to("hub", 0, 10).unwrap().len(), 1);
        assert_eq!(outbox.len().unwrap(), 2);
    }

    #[test]
    fn prune_drops_old_and_sent_rows_and_their_markers() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let outbox = Outbox::new(&store);
        let old = queue(&store, "old", "2026-01-01");
        queue(&store, "new", "2026-09-26");
        crate::db::sync_state::SyncState::new(&store)
            .record_sends("peer", &[old], "2026-09-26")
            .unwrap();

        assert_eq!(outbox.prune("2026-06-01", "hub").unwrap(), 1);
        assert_eq!(outbox.len().unwrap(), 1);
        let markers = crate::testing::count(&store, crate::testing::Table::SyncSends).unwrap();
        assert_eq!(markers, 0);
    }
}
