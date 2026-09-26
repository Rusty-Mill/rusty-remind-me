//! Storage for memory rows: every insert, sync upsert and field update of
//! `memories` that used to be written inline by a domain module.
//!
//! This is ADR-0023's phase 1, step 1. Every write goes through
//! `db::derived::write_memory`, which keeps the derived data (the FTS index,
//! the tag index and the sync outbox) in step, as triggers did up to schema
//! v30 (step 7). The rules stay with their modules: what a capture, a promotion or
//! a consolidation writes, and how vitality is seeded, are decided there and
//! handed over as a [`NewMemory`] or a field value.

use crate::db::derived::{memory_ids, write_memory, Origin};
use crate::db::queries::{parse_memory_row, MEMORY_COLUMNS};
use crate::models::Memory;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Result};
use serde_json::Value;

/// A whole `memories` row, as a writer supplies it.
///
/// [`NewMemory::new`] fills every column with the schema's own default, so a
/// writer names only the columns it sets, and the row matches what an
/// `INSERT` naming just those columns used to produce.
#[derive(Debug, Clone, PartialEq)]
pub struct NewMemory {
    pub id: String,
    pub content: String,
    pub category: String,
    pub tags: Vec<String>,
    pub source: String,
    pub metadata: Value,
    pub created_at: String,
    pub updated_at: String,
    pub capture_id: Option<String>,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub superseded_by: Option<String>,
    pub decay_rate: f64,
    pub vitality: f64,
    pub base_weight: f64,
    pub access_count: i64,
    pub accessed_at: Option<String>,
    pub doc_id: Option<String>,
    pub chunk_index: Option<i64>,
    pub remind_at: Option<String>,
    pub sensitive: bool,
    pub memory_type: String,
    pub status: String,
    pub node_id: Option<String>,
    pub client: String,
    pub source_capture_id: Option<String>,
    pub deleted_at: Option<String>,
}

impl NewMemory {
    /// A row holding `content` under `id`, created and updated at `now`, with
    /// every other column at the schema's default (`schema_tables.sql`).
    pub fn new(id: impl Into<String>, content: impl Into<String>, now: &str) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            category: "general".to_string(),
            tags: Vec::new(),
            source: "manual".to_string(),
            metadata: Value::Object(serde_json::Map::new()),
            created_at: now.to_string(),
            updated_at: now.to_string(),
            capture_id: None,
            subject: None,
            predicate: None,
            object: None,
            superseded_by: None,
            decay_rate: 0.1,
            vitality: 1.0,
            base_weight: 1.0,
            access_count: 0,
            accessed_at: None,
            doc_id: None,
            chunk_index: None,
            remind_at: None,
            sensitive: false,
            memory_type: "unclassified".to_string(),
            status: "active".to_string(),
            node_id: None,
            client: "unknown".to_string(),
            source_capture_id: None,
            deleted_at: None,
        }
    }
}

/// What a sync merge needs of the local copy of a memory.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncView {
    pub tags: Vec<String>,
    pub metadata: Value,
    pub updated_at: String,
}

/// What recording an access needs of a memory.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessInputs {
    pub id: String,
    pub access_count: i64,
    pub decay_rate: f64,
    pub base_weight: f64,
}

/// A memory's subject-predicate-object triple, all three present.
#[derive(Debug, Clone, PartialEq)]
pub struct Triple {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

/// A live memory as the wiki's compile brief shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatedMemory {
    pub id: String,
    pub category: String,
    pub content: String,
    pub created_at: String,
}

/// The columns an insert writes, in [`insert_values`]' order.
const INSERT_COLUMNS: &str = "id, content, category, tags, source, metadata, created_at, \
     updated_at, capture_id, subject, predicate, object, superseded_by, decay_rate, vitality, \
     base_weight, access_count, accessed_at, doc_id, chunk_index, remind_at, sensitive, \
     memory_type, status, node_id, client, source_capture_id, deleted_at";

/// One placeholder per entry of [`INSERT_COLUMNS`].
const INSERT_PLACEHOLDERS: &str =
    "?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?";

/// `row`'s values in [`INSERT_COLUMNS`]' order.
fn insert_values(row: &NewMemory) -> Vec<SqlValue> {
    let text = |s: &str| SqlValue::Text(s.to_string());
    let opt_text = |s: &Option<String>| s.as_deref().map_or(SqlValue::Null, text);
    vec![
        text(&row.id),
        text(&row.content),
        text(&row.category),
        SqlValue::Text(tags_json(&row.tags)),
        text(&row.source),
        SqlValue::Text(row.metadata.to_string()),
        text(&row.created_at),
        text(&row.updated_at),
        opt_text(&row.capture_id),
        opt_text(&row.subject),
        opt_text(&row.predicate),
        opt_text(&row.object),
        opt_text(&row.superseded_by),
        SqlValue::Real(row.decay_rate),
        SqlValue::Real(row.vitality),
        SqlValue::Real(row.base_weight),
        SqlValue::Integer(row.access_count),
        opt_text(&row.accessed_at),
        opt_text(&row.doc_id),
        row.chunk_index.map_or(SqlValue::Null, SqlValue::Integer),
        opt_text(&row.remind_at),
        SqlValue::Integer(i64::from(row.sensitive)),
        text(&row.memory_type),
        text(&row.status),
        opt_text(&row.node_id),
        text(&row.client),
        opt_text(&row.source_capture_id),
        opt_text(&row.deleted_at),
    ]
}

/// `tags` as the JSON array the `tags` column holds.
fn tags_json(tags: &[String]) -> String {
    serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string())
}

/// The `memories` table, over one connection.
pub struct Memories<'c> {
    conn: &'c Connection,
}

impl<'c> Memories<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Insert `row`, made on this node. An existing id is an error.
    pub fn insert(&self, row: &NewMemory) -> Result<()> {
        write_memory(self.conn, &row.id, Origin::Local, || {
            self.conn.execute(
                &format!("INSERT INTO memories ({INSERT_COLUMNS}) VALUES ({INSERT_PLACEHOLDERS})"),
                params_from_iter(insert_values(row)),
            )
        })?;
        Ok(())
    }

    /// Insert `row` unless its id is already taken. Returns whether it was
    /// inserted.
    pub fn insert_or_ignore(&self, row: &NewMemory) -> Result<bool> {
        let inserted = write_memory(self.conn, &row.id, Origin::Local, || {
            self.conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO memories ({INSERT_COLUMNS}) VALUES ({INSERT_PLACEHOLDERS})"
                ),
                params_from_iter(insert_values(row)),
            )
        })?;
        Ok(inserted > 0)
    }

    /// Write a record a peer sent: insert it, or overwrite the local row.
    ///
    /// An overwrite keeps the local `created_at`, `doc_id` and `chunk_index`:
    /// a peer's record does not carry the last two, and the first never
    /// changes. Never queued for sync: it came from there.
    pub fn upsert_synced(&self, row: &NewMemory) -> Result<()> {
        write_memory(self.conn, &row.id, Origin::Sync, || self.upsert_row(row))?;
        Ok(())
    }

    fn upsert_row(&self, row: &NewMemory) -> Result<usize> {
        self.conn.execute(
            &format!(
                "INSERT INTO memories ({INSERT_COLUMNS}) VALUES ({INSERT_PLACEHOLDERS})
                 ON CONFLICT(id) DO UPDATE SET
                    content = excluded.content,
                    category = excluded.category,
                    tags = excluded.tags,
                    source = excluded.source,
                    metadata = excluded.metadata,
                    updated_at = excluded.updated_at,
                    capture_id = excluded.capture_id,
                    node_id = excluded.node_id,
                    client = excluded.client,
                    accessed_at = excluded.accessed_at,
                    access_count = excluded.access_count,
                    decay_rate = excluded.decay_rate,
                    vitality = excluded.vitality,
                    base_weight = excluded.base_weight,
                    status = excluded.status,
                    memory_type = excluded.memory_type,
                    source_capture_id = excluded.source_capture_id,
                    subject = excluded.subject,
                    predicate = excluded.predicate,
                    object = excluded.object,
                    superseded_by = excluded.superseded_by,
                    deleted_at = excluded.deleted_at,
                    sensitive = excluded.sensitive,
                    remind_at = excluded.remind_at"
            ),
            params_from_iter(insert_values(row)),
        )
    }

    /// The local copy of `id` as a sync merge sees it, if there is one.
    /// Unparseable `tags` read as none and unparseable `metadata` as `{}`.
    pub fn sync_view(&self, id: &str) -> Result<Option<SyncView>> {
        self.conn
            .query_row(
                "SELECT tags, metadata, updated_at FROM memories WHERE id = ?",
                params![id],
                |row| {
                    let tags_json: String = row.get(0)?;
                    let metadata_json: String = row.get(1)?;
                    Ok(SyncView {
                        tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                        metadata: serde_json::from_str(&metadata_json)
                            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
                        updated_at: row.get(2)?,
                    })
                },
            )
            .optional()
    }

    /// Replace `id`'s tags and metadata without stamping `updated_at`: a sync
    /// merge that lost last-write-wins still keeps the union, and must not
    /// look like a newer local edit.
    pub fn set_tags_and_metadata(&self, id: &str, tags: &[String], metadata: &Value) -> Result<()> {
        write_memory(self.conn, id, Origin::Sync, || {
            self.conn.execute(
                "UPDATE memories SET tags = ?, metadata = ? WHERE id = ?",
                params![tags_json(tags), metadata.to_string(), id],
            )
        })?;
        Ok(())
    }

    /// Point `id` at the memory that replaces it. `updated_at`, when given,
    /// is stamped too, which is what puts the change in the sync outbox.
    pub fn set_superseded_by(
        &self,
        id: &str,
        superseded_by: &str,
        updated_at: Option<&str>,
    ) -> Result<()> {
        write_memory(self.conn, id, Origin::Local, || match updated_at {
            Some(stamp) => self.conn.execute(
                "UPDATE memories SET superseded_by = ?, updated_at = ? WHERE id = ?",
                params![superseded_by, stamp, id],
            ),
            None => self.conn.execute(
                "UPDATE memories SET superseded_by = ? WHERE id = ?",
                params![superseded_by, id],
            ),
        })?;
        Ok(())
    }

    /// Supersede every live, not yet superseded chunk of the import
    /// `old_import_id` with `new_import_id`, stamping `updated_at`. Returns
    /// how many were superseded.
    pub fn supersede_import(
        &self,
        old_import_id: &str,
        new_import_id: &str,
        updated_at: &str,
    ) -> Result<usize> {
        let ids = memory_ids(
            self.conn,
            "SELECT id FROM memories
              WHERE superseded_by IS NULL
                AND deleted_at IS NULL
                AND json_extract(metadata, '$.import_id') = ?",
            params![old_import_id],
        )?;
        for id in &ids {
            self.set_superseded_by(id, new_import_id, Some(updated_at))?;
        }
        Ok(ids.len())
    }

    /// Hard-delete every memory of `category` belonging to the capture
    /// `capture_id`. Returns how many went.
    pub fn delete_capture_category(&self, capture_id: &str, category: &str) -> Result<usize> {
        let ids = memory_ids(
            self.conn,
            "SELECT id FROM memories WHERE capture_id = ? AND category = ?",
            params![capture_id, category],
        )?;
        for id in &ids {
            write_memory(self.conn, id, Origin::Local, || {
                self.conn
                    .execute("DELETE FROM memories WHERE id = ?", params![id])
            })?;
        }
        Ok(ids.len())
    }

    /// Rewrite `id` as the merge of its cluster: content, summed access
    /// count and tags, stamping `updated_at`.
    pub fn set_merged(
        &self,
        id: &str,
        content: &str,
        access_count: i64,
        tags: &[String],
        updated_at: &str,
    ) -> Result<()> {
        write_memory(self.conn, id, Origin::Local, || {
            self.conn.execute(
                "UPDATE memories SET content = ?, access_count = ?, tags = ?, updated_at = ? WHERE id = ?",
                params![content, access_count, tags_json(tags), updated_at, id],
            )
        })?;
        Ok(())
    }

    /// Set `id`'s vitality and status, without stamping `updated_at`: both
    /// are local scores, not edits.
    pub fn set_vitality(&self, id: &str, vitality: f64, status: &str) -> Result<()> {
        write_memory(self.conn, id, Origin::Local, || {
            self.conn.execute(
                "UPDATE memories SET vitality = ?, status = ? WHERE id = ?",
                params![vitality, status, id],
            )
        })?;
        Ok(())
    }

    /// The access-tracking inputs of each of `ids` that exists, in no
    /// particular order.
    pub fn access_inputs(&self, ids: &[String]) -> Result<Vec<AccessInputs>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, access_count, decay_rate, base_weight FROM memories WHERE id IN ({placeholders})"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(ids.iter()), |row| {
                Ok(AccessInputs {
                    id: row.get(0)?,
                    access_count: row.get(1)?,
                    decay_rate: row.get(2)?,
                    base_weight: row.get(3)?,
                })
            })?
            .collect();
        rows
    }

    /// Record an access to `id`: when, the new count, and the vitality and
    /// status it leads to. Not an edit, so `updated_at` is left alone.
    pub fn record_access(
        &self,
        id: &str,
        accessed_at: &str,
        access_count: i64,
        vitality: f64,
        status: &str,
    ) -> Result<()> {
        // Cached: a search records an access for every result it returns.
        write_memory(self.conn, id, Origin::Local, || {
            self.conn
                .prepare_cached(
                    "UPDATE memories SET accessed_at = ?, access_count = ?, vitality = ?, status = ?
                      WHERE id = ?",
                )?
                .execute(params![accessed_at, access_count, vitality, status, id])
        })?;
        Ok(())
    }

    /// The triple of every live, unsuperseded memory other than `except_id`
    /// that has all three parts.
    pub fn live_triples_except(&self, except_id: &str) -> Result<Vec<Triple>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, subject, predicate, object FROM memories
              WHERE id != ?
                AND superseded_by IS NULL AND deleted_at IS NULL
                AND subject IS NOT NULL AND predicate IS NOT NULL AND object IS NOT NULL",
        )?;
        let rows = stmt
            .query_map(params![except_id], |row| {
                Ok(Triple {
                    id: row.get(0)?,
                    subject: row.get(1)?,
                    predicate: row.get(2)?,
                    object: row.get(3)?,
                })
            })?
            .collect();
        rows
    }

    /// Live, unsuperseded memories created after `cutoff`, oldest first, at
    /// most `limit`.
    pub fn live_created_after(&self, cutoff: &str, limit: usize) -> Result<Vec<CreatedMemory>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, category, content, created_at FROM memories
              WHERE superseded_by IS NULL AND deleted_at IS NULL AND created_at > ?
              ORDER BY created_at ASC LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![cutoff, limit as i64], |r| {
                Ok(CreatedMemory {
                    id: r.get(0)?,
                    category: r.get(1)?,
                    content: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect();
        rows
    }

    /// How many live, unsuperseded memories were created after `cutoff`.
    pub fn count_live_created_after(&self, cutoff: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM memories
              WHERE superseded_by IS NULL AND deleted_at IS NULL AND created_at > ?",
            params![cutoff],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// The memories `ids` names that exist, in no particular order.
    pub fn get_many(&self, ids: &[String]) -> Result<Vec<Memory>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; ids.len()].join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {MEMORY_COLUMNS} FROM memories WHERE id IN ({marks})"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(ids.iter()), parse_memory_row)?
            .collect();
        rows
    }

    /// Whether `id` exists.
    pub fn exists(&self, id: &str) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM memories WHERE id = ?", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(found.is_some())
    }

    /// Whether `id` is marked sensitive, or `None` when there is no such
    /// memory.
    pub fn sensitivity(&self, id: &str) -> Result<Option<bool>> {
        self.conn
            .query_row(
                "SELECT sensitive FROM memories WHERE id = ?",
                params![id],
                |r| r.get(0),
            )
            .optional()
    }

    /// Set `metadata.ingest` to `marker` on every chunk of the import
    /// `doc_id`. Returns how many were stamped.
    pub fn set_ingest_marker(&self, doc_id: &str, marker: &str) -> Result<usize> {
        let ids = memory_ids(
            self.conn,
            "SELECT id FROM memories WHERE doc_id = ?",
            params![doc_id],
        )?;
        for id in &ids {
            write_memory(self.conn, id, Origin::Local, || {
                self.conn.execute(
                    "UPDATE memories SET metadata = json_set(metadata, '$.ingest', ?) WHERE id = ?",
                    params![marker, id],
                )
            })?;
        }
        Ok(ids.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    /// Every column of `id`'s row, as SQLite values, in table order.
    fn raw_row(conn: &Connection, id: &str) -> Vec<SqlValue> {
        let mut stmt = conn.prepare("SELECT * FROM memories WHERE id = ?").unwrap();
        let width = stmt.column_count();
        stmt.query_row(params![id], |row| {
            (0..width).map(|i| row.get::<_, SqlValue>(i)).collect()
        })
        .unwrap()
    }

    #[test]
    fn new_fills_every_column_with_the_schema_default() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        conn.execute(
            "INSERT INTO memories (id, content, created_at, updated_at) VALUES ('a', 'x', ?, ?)",
            params![NOW, NOW],
        )
        .unwrap();
        Memories::new(&conn)
            .insert(&NewMemory::new("b", "x", NOW))
            .unwrap();

        let mut defaulted = raw_row(&conn, "a");
        let mut written = raw_row(&conn, "b");
        defaulted.remove(0);
        written.remove(0);
        assert_eq!(written, defaulted);
    }

    #[test]
    fn insert_refuses_a_taken_id_and_insert_or_ignore_reports_it() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let memories = Memories::new(&conn);
        let row = NewMemory::new("a", "first", NOW);

        assert!(memories.insert_or_ignore(&row).unwrap());
        assert!(memories.insert(&row).is_err());
        let second = NewMemory::new("a", "second", NOW);
        assert!(!memories.insert_or_ignore(&second).unwrap());
        let content: String = conn
            .query_row("SELECT content FROM memories WHERE id = 'a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(content, "first");
    }

    #[test]
    fn a_synced_overwrite_keeps_created_at_and_the_chunk_position() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let memories = Memories::new(&conn);
        memories
            .insert(&NewMemory {
                doc_id: Some("imp_1".to_string()),
                chunk_index: Some(3),
                ..NewMemory::new("a", "local", NOW)
            })
            .unwrap();

        let later = "2026-09-27T00:00:00+00:00";
        memories
            .upsert_synced(&NewMemory {
                created_at: later.to_string(),
                tags: vec!["peer".to_string()],
                ..NewMemory::new("a", "remote", later)
            })
            .unwrap();

        let (content, created, doc, chunk, tags): (String, String, String, i64, String) = conn
            .query_row(
                "SELECT content, created_at, doc_id, chunk_index, tags FROM memories WHERE id = 'a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(content, "remote");
        assert_eq!(created, NOW);
        assert_eq!(doc, "imp_1");
        assert_eq!(chunk, 3);
        assert_eq!(tags, r#"["peer"]"#);
    }

    #[test]
    fn sync_view_reads_tags_that_are_not_an_array_as_none() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        conn.execute(
            r#"INSERT INTO memories (id, content, tags, created_at, updated_at)
             VALUES ('a', 'x', '"not an array"', ?, ?)"#,
            params![NOW, NOW],
        )
        .unwrap();
        let view = Memories::new(&conn).sync_view("a").unwrap().unwrap();
        assert!(view.tags.is_empty());
        assert_eq!(view.metadata, serde_json::json!({}));
        assert!(Memories::new(&conn).sync_view("missing").unwrap().is_none());
    }

    #[test]
    fn access_inputs_skips_unknown_ids_and_takes_an_empty_list() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let memories = Memories::new(&conn);
        memories.insert(&NewMemory::new("a", "x", NOW)).unwrap();

        assert!(memories.access_inputs(&[]).unwrap().is_empty());
        let found = memories
            .access_inputs(&["a".to_string(), "missing".to_string()])
            .unwrap();
        assert_eq!(
            found,
            vec![AccessInputs {
                id: "a".to_string(),
                access_count: 0,
                decay_rate: 0.1,
                base_weight: 1.0,
            }]
        );
    }
}
