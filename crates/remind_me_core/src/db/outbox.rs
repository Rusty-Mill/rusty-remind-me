//! Storage for the sync outbox: `sync_outbox` rows (one per local change to
//! push) and their `sync_sends` markers, on the engine's memories core
//! (`db::engine::outbox`).
//!
//! The rows themselves are queued by the repositories as they write, with
//! the write's [`crate::db::derived::Origin`] deciding whether. The rules
//! stay in [`crate::sync`]: when sync is enabled, the retention window,
//! batch size, and how a payload decodes.

use super::engine::{self, EngineLock};
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

/// The outbox, on the engine's memories core.
pub struct Outbox<'c> {
    core: &'c EngineLock,
}

impl<'c> Outbox<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    /// Up to `limit` unsent rows above `after_id` not yet sent to
    /// `remote_id`, oldest first.
    pub fn unsent_to(
        &self,
        remote_id: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<OutboxEntry>> {
        engine::outbox::unsent_to(&self.core.lock(), remote_id, after_id, limit)
    }

    /// How many rows have no send recorded to `remote_id`, and the oldest
    /// one's `created_at`.
    pub fn pending_for(&self, remote_id: &str) -> Result<(i64, Option<String>)> {
        engine::outbox::pending_for(&self.core.lock(), remote_id)
    }

    /// How many rows the outbox holds.
    pub fn len(&self) -> Result<i64> {
        engine::outbox::len(&self.core.lock())
    }

    /// How many rows are not yet marked sent.
    pub fn unsent_count(&self) -> Result<i64> {
        engine::outbox::unsent_count(&self.core.lock())
    }

    /// Whether the outbox is empty.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(engine::outbox::len(&self.core.lock())? == 0)
    }

    /// Delete every sent row, every row `done_by` has taken, and every row
    /// created before `cutoff`, then the send markers left pointing at
    /// nothing. Returns how many rows went.
    pub fn prune(&self, cutoff: &str, done_by: &str) -> Result<usize> {
        engine::outbox::prune(&mut self.core.lock(), cutoff, done_by)
    }

    /// Empty the outbox and its send markers.
    pub fn clear(&self) -> Result<()> {
        engine::outbox::clear(&mut self.core.lock())
    }

    /// Queue every memory, entity and mention link as an insert, stamped
    /// now: what a node that just turned sync on owes its remotes.
    pub fn backfill_everything(&self) -> Result<()> {
        engine::outbox::backfill(&mut self.core.lock())
    }
}

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
    fn the_outbox_sends_and_backfill_agree() {
        use crate::db::entities::Entities;
        use crate::db::memories::{Memories, NewMemory};
        use crate::db::sync_state::SyncState;
        use crate::entity::Entity;
        const NOW: &str = "2026-09-26T00:00:00+00:00";

        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let state = SyncState::new(&store);
        let outbox = Outbox::new(&store);
        let memories = Memories::new(&store);
        memories.insert(&NewMemory::new("off", "x", NOW)).unwrap();
        assert!(
            outbox.is_empty().unwrap(),
            "nothing is queued while sync is off"
        );

        state.set_flag("sync_enabled", "1").unwrap();
        assert_eq!(state.flag("sync_enabled").unwrap().as_deref(), Some("1"));
        memories.insert(&NewMemory::new("a", "x", NOW)).unwrap();
        memories.insert(&NewMemory::new("b", "y", NOW)).unwrap();
        let queued = outbox.unsent_to("hub", 0, 10).unwrap();
        let keys: Vec<&str> = queued.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["a", "b"]);

        state.record_sends("hub", &[queued[0].id], "t1").unwrap();
        assert_eq!(outbox.pending_for("hub").unwrap().0, 1);
        assert_eq!(outbox.pending_for("peer").unwrap().0, 2);
        assert_eq!(outbox.unsent_to("hub", 0, 10).unwrap().len(), 1);
        assert!(outbox
            .unsent_to("peer", queued[1].id, 10)
            .unwrap()
            .is_empty());
        assert_eq!(outbox.unsent_count().unwrap(), 2);

        // The graph queues into the same outbox.
        Entities::new(&store)
            .insert(
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
        assert_eq!(outbox.len().unwrap(), 3);

        outbox.clear().unwrap();
        assert!(outbox.is_empty().unwrap());
        assert_eq!(outbox.pending_for("hub").unwrap(), (0, None));

        outbox.backfill_everything().unwrap();
        // Memories in scan order, which the store does not promise.
        let mut keys: Vec<String> = outbox
            .unsent_to("hub", 0, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();
        keys.sort();
        assert_eq!(keys, ["a", "b", "ent_1", "off"]);
        assert_eq!(outbox.prune("9999", "hub").unwrap(), 4);
        assert!(outbox.is_empty().unwrap());
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
