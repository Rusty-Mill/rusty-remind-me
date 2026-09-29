//! Storage for the sync outbox: `sync_outbox` rows (one per local change to
//! push) and their `sync_sends` markers.
//!
//! ADR-0023 phase 1, step 6. Every statement the sync modules ran against
//! the outbox lives here. The rows themselves are queued by the repositories
//! as they write (`db::derived`, step 7). The rules stay in [`crate::sync`]:
//! when sync is enabled, the retention window, batch size, and how a payload
//! decodes.

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineLock};
use super::{Result, Store};
use rusqlite::{params, Connection, OptionalExtension};

/// SQLite's clock as an RFC 3339 timestamp with microseconds, the shape
/// every stamp in the outbox takes.
const NOW_ISO_EXPR: &str = "strftime('%Y-%m-%dT%H:%M:%f000', 'now') || '+00:00'";

/// One outbox row as a push reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEntry {
    pub id: i64,
    /// The `memory_id` column: the key the row was queued under, which for a
    /// link is its memory's id.
    pub key: String,
    pub payload_json: String,
}

/// The outbox, over one connection, or on the engine's memories core when
/// the store's tables hold it (`db::engine::outbox`).
pub struct Outbox<'c> {
    conn: &'c Connection,
    #[cfg(feature = "engine-store")]
    core: Option<&'c EngineLock>,
}

impl<'c> Outbox<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            #[cfg(feature = "engine-store")]
            core: store.core(),
        }
    }

    /// Up to `limit` unsent rows above `after_id` not yet sent to
    /// `remote_id`, oldest first.
    pub fn unsent_to(
        &self,
        remote_id: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<OutboxEntry>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::outbox::unsent_to(&core.lock(), remote_id, after_id, limit);
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, memory_id, payload FROM sync_outbox
              WHERE id > ?1 AND sent_at = ''
                AND id NOT IN (SELECT outbox_id FROM sync_sends WHERE remote_id = ?2)
              ORDER BY id ASC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![after_id, remote_id, limit as i64], |row| {
                Ok(OutboxEntry {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    payload_json: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// How many rows have no send recorded to `remote_id`, and the oldest
    /// one's `created_at`.
    pub fn pending_for(&self, remote_id: &str) -> Result<(i64, Option<String>)> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::outbox::pending_for(&core.lock(), remote_id);
        }
        Ok(self.conn.query_row(
            "SELECT COUNT(*), MIN(created_at) FROM sync_outbox o
              WHERE NOT EXISTS (
                  SELECT 1 FROM sync_sends s
                   WHERE s.outbox_id = o.id AND s.remote_id = ?
              )",
            params![remote_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// How many rows the outbox holds.
    pub fn len(&self) -> Result<i64> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::outbox::len(&core.lock());
        }
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM sync_outbox", [], |r| r.get(0))?)
    }

    /// How many rows are not yet marked sent.
    pub fn unsent_count(&self) -> Result<i64> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::outbox::unsent_count(&core.lock());
        }
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM sync_outbox WHERE sent_at = ''",
            [],
            |r| r.get(0),
        )?)
    }

    /// Whether the outbox is empty.
    pub fn is_empty(&self) -> Result<bool> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return Ok(engine::outbox::len(&core.lock())? == 0);
        }
        let any: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM sync_outbox LIMIT 1", [], |r| r.get(0))
            .optional()?;
        Ok(any.is_none())
    }

    /// Delete every sent row, every row `done_by` has taken, and every row
    /// created before `cutoff`, then the send markers left pointing at
    /// nothing. Returns how many rows went.
    pub fn prune(&self, cutoff: &str, done_by: &str) -> Result<usize> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::outbox::prune(&mut core.lock(), cutoff, done_by);
        }
        let removed = self.conn.execute(
            "DELETE FROM sync_outbox WHERE sent_at != '' OR created_at < ?1 \
             OR id IN (SELECT outbox_id FROM sync_sends WHERE remote_id = ?2)",
            params![cutoff, done_by],
        )?;
        self.conn.execute(
            "DELETE FROM sync_sends WHERE outbox_id NOT IN (SELECT id FROM sync_outbox)",
            [],
        )?;
        Ok(removed)
    }

    /// Empty the outbox and its send markers.
    pub fn clear(&self) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::outbox::clear(&mut core.lock());
        }
        Ok(self
            .conn
            .execute_batch("DELETE FROM sync_outbox; DELETE FROM sync_sends;")?)
    }

    /// Queue every memory, entity and mention link as an insert, stamped
    /// with SQLite's clock: what a node that just turned sync on owes its
    /// remotes. Payloads have the shape `db::derived` queues.
    pub fn backfill_everything(&self) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::outbox::backfill(&mut core.lock());
        }
        Ok(self.conn.execute_batch(&format!(
            "INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
             SELECT id, 'insert', json_object(
                 'id', id, 'content', content, 'category', category, 'tags', tags,
                 'source', source, 'metadata', metadata, 'created_at', created_at,
                 'updated_at', updated_at, 'capture_id', capture_id, 'node_id', node_id,
                 'client', client, 'accessed_at', accessed_at, 'access_count', access_count,
                 'decay_rate', decay_rate, 'vitality', vitality, 'base_weight', base_weight,
                 'status', status, 'memory_type', memory_type,
                 'source_capture_id', source_capture_id, 'subject', subject,
                 'predicate', predicate, 'object', object, 'superseded_by', superseded_by,
                 'doc_id', doc_id, 'chunk_index', chunk_index, 'deleted_at', deleted_at
             ), {now}
             FROM memories;

             INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
             SELECT id, 'insert', json_object(
                 'record_type', 'entity', 'id', id, 'name', name, 'kind', kind,
                 'aliases', aliases, 'created_at', created_at, 'updated_at', updated_at,
                 'node_id', node_id
             ), {now}
             FROM entities;

             INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
             SELECT memory_id, 'insert', json_object(
                 'record_type', 'memory_entity',
                 'id', memory_id || '|' || entity_id,
                 'memory_id', memory_id, 'entity_id', entity_id, 'created_at', created_at
             ), {now}
             FROM memory_entities;",
            now = NOW_ISO_EXPR
        ))?)
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
    fn the_outbox_sends_and_backfill_agree_on_each_backend() {
        use crate::db::entities::Entities;
        use crate::db::memories::{Memories, NewMemory};
        use crate::db::sync_state::SyncState;
        use crate::entity::Entity;
        const NOW: &str = "2026-09-26T00:00:00+00:00";

        crate::db::on_each_backend(|db| {
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

            // The graph stays on SQLite, but queues into the same outbox.
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
            // Memories in scan order, which neither backend promises.
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
        });
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
