//! Storage for the sync outbox: `sync_outbox` rows (one per local change to
//! push) and their `sync_sends` markers.
//!
//! ADR-0023 phase 1, step 6. Every statement the sync modules ran against
//! the outbox lives here. The rows themselves are queued by the repositories
//! as they write (`db::derived`, step 7). The rules stay in [`crate::sync`]:
//! when sync is enabled, the retention window, batch size, and how a payload
//! decodes.

use rusqlite::{params, Connection, OptionalExtension, Result};

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

/// The outbox, over one connection.
pub struct Outbox<'c> {
    conn: &'c Connection,
}

impl<'c> Outbox<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Up to `limit` unsent rows above `after_id` not yet sent to
    /// `remote_id`, oldest first.
    pub fn unsent_to(
        &self,
        remote_id: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<OutboxEntry>> {
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
            .collect();
        rows
    }

    /// How many rows have no send recorded to `remote_id`, and the oldest
    /// one's `created_at`.
    pub fn pending_for(&self, remote_id: &str) -> Result<(i64, Option<String>)> {
        self.conn.query_row(
            "SELECT COUNT(*), MIN(created_at) FROM sync_outbox o
              WHERE NOT EXISTS (
                  SELECT 1 FROM sync_sends s
                   WHERE s.outbox_id = o.id AND s.remote_id = ?
              )",
            params![remote_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    /// How many rows the outbox holds.
    pub fn len(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM sync_outbox", [], |r| r.get(0))
    }

    /// How many rows are not yet marked sent.
    pub fn unsent_count(&self) -> Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM sync_outbox WHERE sent_at = ''",
            [],
            |r| r.get(0),
        )
    }

    /// Whether the outbox is empty.
    pub fn is_empty(&self) -> Result<bool> {
        let any: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM sync_outbox LIMIT 1", [], |r| r.get(0))
            .optional()?;
        Ok(any.is_none())
    }

    /// Delete every sent row and every row created before `cutoff`, then
    /// the send markers left pointing at nothing. Returns how many rows went.
    pub fn prune(&self, cutoff: &str) -> Result<usize> {
        let removed = self.conn.execute(
            "DELETE FROM sync_outbox WHERE sent_at != '' OR created_at < ?",
            params![cutoff],
        )?;
        self.conn.execute(
            "DELETE FROM sync_sends WHERE outbox_id NOT IN (SELECT id FROM sync_outbox)",
            [],
        )?;
        Ok(removed)
    }

    /// Empty the outbox and its send markers.
    pub fn clear(&self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM sync_outbox; DELETE FROM sync_sends;")
    }

    /// Queue every memory, entity and mention link as an insert, stamped
    /// with SQLite's clock: what a node that just turned sync on owes its
    /// remotes. Payloads have the shape `db::derived` queues.
    pub fn backfill_everything(&self) -> Result<()> {
        self.conn.execute_batch(&format!(
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
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn queue(conn: &Connection, key: &str, created_at: &str) -> i64 {
        conn.execute(
            "INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
             VALUES (?, 'insert', '{}', ?)",
            params![key, created_at],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn pending_counts_rows_with_no_send_to_the_remote() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let outbox = Outbox::new(&conn);
        assert!(outbox.is_empty().unwrap());
        let first = queue(&conn, "a", "2026-09-01");
        queue(&conn, "b", "2026-09-02");
        crate::db::sync_state::SyncState::new(&conn)
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
        let conn = db.conn();
        let outbox = Outbox::new(&conn);
        let old = queue(&conn, "old", "2026-01-01");
        queue(&conn, "new", "2026-09-26");
        crate::db::sync_state::SyncState::new(&conn)
            .record_sends("hub", &[old], "2026-09-26")
            .unwrap();

        assert_eq!(outbox.prune("2026-06-01").unwrap(), 1);
        assert_eq!(outbox.len().unwrap(), 1);
        let markers: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_sends", [], |r| r.get(0))
            .unwrap();
        assert_eq!(markers, 0);
    }
}
