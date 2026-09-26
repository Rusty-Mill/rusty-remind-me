//! Derived data the repositories keep in step with each write: the
//! full-text indexes, the tag index, and the sync outbox.
//!
//! ADR-0023 phase 1, step 7. Up to schema v30, fifteen SQLite triggers did
//! this. They fired on every statement, whoever ran it, and could not tell a
//! local edit from a record a peer had sent, so every synced write queued an
//! outbox row that then had to be found and marked sent again. The
//! repositories do it now, with the write's [`Origin`] deciding whether the
//! outbox hears about it.
//!
//! The payloads are still built by SQLite's `json_object` from the row as
//! written, with the column list the triggers used, so a peer or hub reads
//! exactly what it read before: tags and metadata as JSON strings, and
//! `sensitive` as 0 or 1.

use rusqlite::{params, Connection, OptionalExtension, Result};

/// Where a write came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Made on this node. Queued for sync, when sync is enabled.
    Local,
    /// Applied from a record a peer or the hub sent. Never queued: sending
    /// it back would make two nodes chase each other.
    Sync,
}

/// SQLite's clock as an RFC 3339 timestamp with microseconds, the shape
/// every outbox stamp takes.
const NOW_ISO: &str = "strftime('%Y-%m-%dT%H:%M:%f000', 'now') || '+00:00'";

/// Whether sync is on, as the outbox gate reads it.
const SYNC_ENABLED: &str =
    "COALESCE((SELECT value FROM sync_flags WHERE key = 'sync_enabled'), '0') = '1'";

/// A memory's outbox payload, from the row aliased `m`.
const MEMORY_PAYLOAD: &str = "json_object('id', m.id, 'content', m.content, \
     'category', m.category, 'tags', m.tags, 'source', m.source, 'metadata', m.metadata, \
     'created_at', m.created_at, 'updated_at', m.updated_at, 'capture_id', m.capture_id, \
     'node_id', m.node_id, 'client', m.client, 'accessed_at', m.accessed_at, \
     'access_count', m.access_count, 'decay_rate', m.decay_rate, 'vitality', m.vitality, \
     'base_weight', m.base_weight, 'status', m.status, 'memory_type', m.memory_type, \
     'source_capture_id', m.source_capture_id, 'subject', m.subject, \
     'predicate', m.predicate, 'object', m.object, 'superseded_by', m.superseded_by, \
     'doc_id', m.doc_id, 'chunk_index', m.chunk_index, 'deleted_at', m.deleted_at, \
     'remind_at', m.remind_at, 'sensitive', m.sensitive)";

/// Run `body` inside a savepoint: all of it lands, or none of it does, as a
/// statement and its triggers did.
fn atomically<T>(conn: &Connection, body: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch("SAVEPOINT derived;")?;
    match body() {
        Ok(value) => {
            conn.execute_batch("RELEASE derived;")?;
            Ok(value)
        }
        Err(e) => {
            conn.execute_batch("ROLLBACK TO derived; RELEASE derived;")?;
            Err(e)
        }
    }
}

// --- memories -------------------------------------------------------------

fn memory_updated_at(conn: &Connection, id: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT updated_at FROM memories WHERE id = ?",
        params![id],
        |r| r.get(0),
    )
    .optional()
}

/// Take memory `id` out of the full-text index, as it is stored now. A
/// no-op when there is no such memory.
fn unindex_memory(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO memories_fts(memories_fts, rowid, content, category, tags)
         SELECT 'delete', rowid, content, category, tags FROM memories WHERE id = ?",
        params![id],
    )?;
    Ok(())
}

/// Put memory `id` into the full-text index and rebuild its tag rows, as it
/// is stored now.
fn index_memory(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO memories_fts(rowid, content, category, tags)
         SELECT rowid, content, category, tags FROM memories WHERE id = ?",
        params![id],
    )?;
    conn.execute("DELETE FROM memory_tags WHERE memory_id = ?", params![id])?;
    conn.execute(
        "INSERT OR IGNORE INTO memory_tags (memory_id, tag)
         SELECT m.id, je.value
           FROM memories m, json_each(m.tags) AS je
          WHERE m.id = ? AND typeof(je.value) = 'text' AND json_valid(m.tags)",
        params![id],
    )?;
    Ok(())
}

/// Queue memory `id`, as stored now, for sync, when sync is enabled.
fn queue_memory(conn: &Connection, id: &str, operation: &str) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
             SELECT m.id, ?, {MEMORY_PAYLOAD}, {NOW_ISO}
               FROM memories m
              WHERE m.id = ? AND {SYNC_ENABLED}"
        ),
        params![operation, id],
    )?;
    Ok(())
}

/// Run `write`, which inserts, updates or deletes at most the memory `id`,
/// and keep that memory's derived data in step:
///
/// - the full-text index and tag rows follow the row, or go with it;
/// - a local write that created the row queues an `insert`, and one that
///   moved `updated_at` queues an `update`. Any other write (an access
///   stamp, a score) is not an edit, and is not synced.
pub(crate) fn write_memory<T>(
    conn: &Connection,
    id: &str,
    origin: Origin,
    write: impl FnOnce() -> Result<T>,
) -> Result<T> {
    atomically(conn, || {
        let before = memory_updated_at(conn, id)?;
        unindex_memory(conn, id)?;
        let result = write()?;
        let after = memory_updated_at(conn, id)?;
        match &after {
            // Gone: the index entry went above, and the tag rows go here.
            None => {
                conn.execute("DELETE FROM memory_tags WHERE memory_id = ?", params![id])?;
            }
            Some(stamp) => {
                index_memory(conn, id)?;
                if origin == Origin::Local {
                    match &before {
                        None => queue_memory(conn, id, "insert")?,
                        Some(old) if old != stamp => queue_memory(conn, id, "update")?,
                        Some(_) => {}
                    }
                }
            }
        }
        Ok(result)
    })
}

/// The ids of the memories `sql` selects with `bindings`, for a write that
/// can touch several and so has to be applied one memory at a time.
pub(crate) fn memory_ids(
    conn: &Connection,
    sql: &str,
    bindings: impl rusqlite::Params,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(bindings, |r| r.get(0))?.collect();
    rows
}

// --- the knowledge graph --------------------------------------------------

/// Queue entity `id`, as stored now, for sync, when sync is enabled.
pub(crate) fn queue_entity(conn: &Connection, id: &str, operation: &str) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
             SELECT e.id, ?, json_object('record_type', 'entity', 'id', e.id,
                    'name', e.name, 'kind', e.kind, 'aliases', e.aliases,
                    'created_at', e.created_at, 'updated_at', e.updated_at,
                    'node_id', e.node_id), {NOW_ISO}
               FROM entities e
              WHERE e.id = ? AND {SYNC_ENABLED}"
        ),
        params![operation, id],
    )?;
    Ok(())
}

/// Queue the new relation `id` for sync, when sync is enabled.
pub(crate) fn queue_relation(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
             SELECT r.id, 'insert', json_object('record_type', 'entity_relation',
                    'id', r.id, 'subject_entity_id', r.subject_entity_id,
                    'relation', r.relation, 'object_entity_id', r.object_entity_id,
                    'created_at', r.created_at, 'updated_at', r.updated_at,
                    'node_id', r.node_id), {NOW_ISO}
               FROM entity_relations r
              WHERE r.id = ? AND {SYNC_ENABLED}"
        ),
        params![id],
    )?;
    Ok(())
}

/// Queue the new mention link for sync, when sync is enabled. Keyed on the
/// memory's id, with `memory_id|entity_id` as the wire id.
pub(crate) fn queue_link(conn: &Connection, memory_id: &str, entity_id: &str) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
             SELECT l.memory_id, 'insert', json_object('record_type', 'memory_entity',
                    'id', l.memory_id || '|' || l.entity_id, 'memory_id', l.memory_id,
                    'entity_id', l.entity_id, 'created_at', l.created_at), {NOW_ISO}
               FROM memory_entities l
              WHERE l.memory_id = ? AND l.entity_id = ? AND {SYNC_ENABLED}"
        ),
        params![memory_id, entity_id],
    )?;
    Ok(())
}

// --- the wiki -------------------------------------------------------------

/// Run `write`, which inserts, updates or deletes at most the wiki page
/// `slug`, and keep the page's full-text index entry in step.
pub(crate) fn write_wiki_page<T>(
    conn: &Connection,
    slug: &str,
    write: impl FnOnce() -> Result<T>,
) -> Result<T> {
    atomically(conn, || {
        conn.execute(
            "INSERT INTO wiki_fts(wiki_fts, rowid, title, content)
             SELECT 'delete', rowid, title, content FROM wiki_pages WHERE slug = ?",
            params![slug],
        )?;
        let result = write()?;
        conn.execute(
            "INSERT INTO wiki_fts(rowid, title, content)
             SELECT rowid, title, content FROM wiki_pages WHERE slug = ?",
            params![slug],
        )?;
        Ok(result)
    })
}

/// Rebuild the full-text indexes and the tag index from the rows.
///
/// Everything here is derived, so a rebuild is always safe. It is how rows
/// written without going through the repositories become searchable: a test
/// fixture planted with raw SQL, or a repair after the indexes were lost.
pub fn rebuild_indexes(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "INSERT INTO memories_fts(memories_fts) VALUES('rebuild');
         INSERT INTO wiki_fts(wiki_fts) VALUES('rebuild');
         DELETE FROM memory_tags;
         INSERT OR IGNORE INTO memory_tags (memory_id, tag)
         SELECT m.id, je.value
           FROM memories m, json_each(m.tags) AS je
          WHERE typeof(je.value) = 'text' AND json_valid(m.tags);",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    const T1: &str = "2026-09-26T00:00:00+00:00";
    const T2: &str = "2026-09-27T00:00:00+00:00";

    fn enable_sync(conn: &Connection) {
        conn.execute(
            "INSERT OR REPLACE INTO sync_flags (key, value) VALUES ('sync_enabled', '1')",
            [],
        )
        .unwrap();
    }

    fn insert(conn: &Connection, id: &str, content: &str, tags: &str, origin: Origin) {
        write_memory(conn, id, origin, || {
            conn.execute(
                "INSERT INTO memories (id, content, tags, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?)",
                params![id, content, tags, T1, T1],
            )
        })
        .unwrap();
    }

    fn fts_hits(conn: &Connection, term: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT m.id FROM memories_fts JOIN memories m ON m.rowid = memories_fts.rowid
                  WHERE memories_fts MATCH ? ORDER BY m.id",
            )
            .unwrap();
        let rows = stmt
            .query_map([term], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<String>>>()
            .unwrap();
        rows
    }

    fn outbox(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT memory_id, operation FROM sync_outbox ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        rows
    }

    #[test]
    fn the_index_and_tags_follow_a_memory_through_update_and_delete() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        insert(&conn, "a", "quokka", r#"["red"]"#, Origin::Local);
        assert_eq!(fts_hits(&conn, "quokka"), vec!["a"]);

        write_memory(&conn, "a", Origin::Local, || {
            conn.execute(
                r#"UPDATE memories SET content = 'wombat', tags = '["blue"]' WHERE id = 'a'"#,
                [],
            )
        })
        .unwrap();
        assert!(fts_hits(&conn, "quokka").is_empty());
        assert_eq!(fts_hits(&conn, "wombat"), vec!["a"]);
        let tags: Vec<String> = conn
            .prepare("SELECT tag FROM memory_tags WHERE memory_id = 'a'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(tags, vec!["blue"]);

        write_memory(&conn, "a", Origin::Local, || {
            conn.execute("DELETE FROM memories WHERE id = 'a'", [])
        })
        .unwrap();
        assert!(fts_hits(&conn, "wombat").is_empty());
        let left: i64 = conn
            .query_row("SELECT count(*) FROM memory_tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn only_local_edits_that_move_updated_at_are_queued() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        enable_sync(&conn);
        insert(&conn, "local", "x", "[]", Origin::Local);
        insert(&conn, "synced", "y", "[]", Origin::Sync);

        // A score change is not an edit.
        write_memory(&conn, "local", Origin::Local, || {
            conn.execute("UPDATE memories SET vitality = 0.5 WHERE id = 'local'", [])
        })
        .unwrap();
        // An edit is.
        write_memory(&conn, "local", Origin::Local, || {
            conn.execute(
                "UPDATE memories SET content = 'z', updated_at = ? WHERE id = 'local'",
                [T2],
            )
        })
        .unwrap();

        assert_eq!(
            outbox(&conn),
            vec![
                ("local".to_string(), "insert".to_string()),
                ("local".to_string(), "update".to_string())
            ]
        );
    }

    #[test]
    fn nothing_is_queued_while_sync_is_off() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        insert(&conn, "a", "x", "[]", Origin::Local);
        assert!(outbox(&conn).is_empty());
    }

    #[test]
    fn a_failed_write_leaves_the_index_as_it_was() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        insert(&conn, "a", "quokka", "[]", Origin::Local);

        let failed = write_memory(&conn, "a", Origin::Local, || -> Result<()> {
            Err(rusqlite::Error::InvalidQuery)
        });
        assert!(failed.is_err());
        assert_eq!(fts_hits(&conn, "quokka"), vec!["a"]);
    }

    #[test]
    fn a_queued_memory_payload_keeps_the_trigger_shape() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        enable_sync(&conn);
        insert(&conn, "a", "x", r#"["t"]"#, Origin::Local);

        let payload: String = conn
            .query_row("SELECT payload FROM sync_outbox", [], |r| r.get(0))
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        // Tags travel as the column's JSON text, and `sensitive` as an integer.
        assert_eq!(payload["tags"], serde_json::json!(r#"["t"]"#));
        assert_eq!(payload["sensitive"], serde_json::json!(0));
        assert_eq!(payload.as_object().unwrap().len(), 28);
    }
}
