//! Memories on SQLite: the [`MemoryStore`] adapter, and the row-level
//! write every other memory-touching adapter goes through.
//!
//! A row is the engine's [`MemoryRow`], every column as SQLite stores it,
//! so the rules the engine applies to a row (what a tombstone empties, the
//! outbox payload, a search's filters) are reused, not rewritten. Every
//! write goes through [`SqliteTables::write_memory`], which keeps the
//! full-text index, the tag table and the outbox in step in one savepoint.

use super::{sqlite_error, SqliteTables};
use crate::db::derived::Origin;
use crate::db::engine::memories::{distinct, is_after, queued_operation, Edit, MemoryRow};
use crate::db::engine::outbox::now_iso;
use crate::db::memories::{
    tags_json, AccessInputs, CreatedMemory, KeywordFilter, ListFilter, MemoryEdit, MemoryStore,
    NewMemory, PageFilter, SyncView, Triple,
};
use crate::db::{Result, StoreError};
use crate::models::{Memory, UnannotatedMemory, UnclassifiedMemory, DIALOG_CATEGORY};
use crate::testing::MEMORY_COLUMNS;
use rusqlite::types::Value as Sql;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use serde_json::Value;
use std::collections::HashSet;

/// `memories` in the v32 column order ([`MEMORY_COLUMNS`]); the full-text
/// index over live rows' (content, category, tags), keyed by rowid as the
/// engine keys its own; the tags each row carries; the outbox; and flags.
/// `memory_entities` is the entity graph's link table, read here by
/// [`MemoryStore::unannotated_page`] and the entity filters.
pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
    id TEXT PRIMARY KEY, content TEXT NOT NULL, category TEXT NOT NULL,
    tags TEXT NOT NULL, source TEXT NOT NULL, metadata TEXT NOT NULL,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL, capture_id TEXT,
    subject TEXT, predicate TEXT, object TEXT, superseded_by TEXT,
    decay_rate REAL NOT NULL, vitality REAL NOT NULL, base_weight REAL NOT NULL,
    access_count INTEGER NOT NULL, accessed_at TEXT, doc_id TEXT,
    chunk_index INTEGER, remind_at TEXT, sensitive INTEGER NOT NULL,
    memory_type TEXT NOT NULL, status TEXT NOT NULL, node_id TEXT,
    client TEXT NOT NULL, source_capture_id TEXT, deleted_at TEXT,
    project TEXT, session_id TEXT, git_remote TEXT, git_branch TEXT,
    git_sha TEXT, cwd TEXT, valid_from TEXT, valid_until TEXT,
    confidence REAL NOT NULL, verified_at TEXT, outcome TEXT,
    written_by TEXT NOT NULL, capture_method TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS memories_created ON memories (created_at, id);
CREATE INDEX IF NOT EXISTS memories_doc ON memories (doc_id);
CREATE INDEX IF NOT EXISTS memories_capture ON memories (capture_id);
CREATE INDEX IF NOT EXISTS memories_source_capture ON memories (source_capture_id);
CREATE INDEX IF NOT EXISTS memories_type ON memories (memory_type);
CREATE INDEX IF NOT EXISTS memories_remind ON memories (remind_at);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5 (
    content, category, tags, content = '', contentless_delete = 1);
CREATE TABLE IF NOT EXISTS memory_tags (
    tag TEXT NOT NULL, memory_id TEXT NOT NULL, PRIMARY KEY (tag, memory_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS memory_tags_memory ON memory_tags (memory_id);
CREATE TABLE IF NOT EXISTS sync_outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT, memory_id TEXT NOT NULL,
    operation TEXT NOT NULL, payload TEXT NOT NULL, created_at TEXT NOT NULL,
    sent_at TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS flags (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS memory_entities (
    memory_id TEXT NOT NULL, entity_id TEXT NOT NULL, created_at TEXT NOT NULL,
    PRIMARY KEY (memory_id, entity_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS memory_entities_entity ON memory_entities (entity_id);
";

/// The flag that turns the outbox on, as `db::derived`'s gate reads it.
const SYNC_ENABLED: &str = "sync_enabled";

/// Every column, comma-separated, for `SELECT` and `INSERT`.
fn columns() -> String {
    MEMORY_COLUMNS.join(", ")
}

/// A `memories` row read in [`MEMORY_COLUMNS`] order.
pub(super) fn memory_row(r: &Row<'_>) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        id: r.get(0)?,
        content: r.get(1)?,
        category: r.get(2)?,
        tags: r.get(3)?,
        source: r.get(4)?,
        metadata: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
        capture_id: r.get(8)?,
        subject: r.get(9)?,
        predicate: r.get(10)?,
        object: r.get(11)?,
        superseded_by: r.get(12)?,
        decay_rate: r.get(13)?,
        vitality: r.get(14)?,
        base_weight: r.get(15)?,
        access_count: r.get(16)?,
        accessed_at: r.get(17)?,
        doc_id: r.get(18)?,
        chunk_index: r.get(19)?,
        remind_at: r.get(20)?,
        sensitive: r.get(21)?,
        memory_type: r.get(22)?,
        status: r.get(23)?,
        node_id: r.get(24)?,
        client: r.get(25)?,
        source_capture_id: r.get(26)?,
        deleted_at: r.get(27)?,
        project: r.get(28)?,
        session_id: r.get(29)?,
        git_remote: r.get(30)?,
        git_branch: r.get(31)?,
        git_sha: r.get(32)?,
        cwd: r.get(33)?,
        valid_from: r.get(34)?,
        valid_until: r.get(35)?,
        confidence: r.get(36)?,
        verified_at: r.get(37)?,
        outcome: r.get(38)?,
        written_by: r.get(39)?,
        capture_method: r.get(40)?,
    })
}

/// `row`'s values in [`MEMORY_COLUMNS`] order, for binding.
fn values(row: &MemoryRow) -> Vec<Sql> {
    let text = |s: &str| Sql::Text(s.to_string());
    let opt = |s: &Option<String>| s.as_deref().map_or(Sql::Null, text);
    vec![
        text(&row.id),
        text(&row.content),
        text(&row.category),
        text(&row.tags),
        text(&row.source),
        text(&row.metadata),
        text(&row.created_at),
        text(&row.updated_at),
        opt(&row.capture_id),
        opt(&row.subject),
        opt(&row.predicate),
        opt(&row.object),
        opt(&row.superseded_by),
        Sql::Real(row.decay_rate),
        Sql::Real(row.vitality),
        Sql::Real(row.base_weight),
        Sql::Integer(row.access_count),
        opt(&row.accessed_at),
        opt(&row.doc_id),
        row.chunk_index.map_or(Sql::Null, Sql::Integer),
        opt(&row.remind_at),
        Sql::Integer(i64::from(row.sensitive)),
        text(&row.memory_type),
        text(&row.status),
        opt(&row.node_id),
        text(&row.client),
        opt(&row.source_capture_id),
        opt(&row.deleted_at),
        opt(&row.project),
        opt(&row.session_id),
        opt(&row.git_remote),
        opt(&row.git_branch),
        opt(&row.git_sha),
        opt(&row.cwd),
        opt(&row.valid_from),
        opt(&row.valid_until),
        Sql::Real(row.confidence),
        opt(&row.verified_at),
        opt(&row.outcome),
        text(&row.written_by),
        text(&row.capture_method),
    ]
}

/// Rows of `memories` matching `clause` (SQL after `WHERE`, `1` for all),
/// in the order `order` gives (SQL after `ORDER BY`).
pub(super) fn rows_where(
    conn: &Connection,
    clause: &str,
    order: &str,
    bind: &[Sql],
) -> Result<Vec<MemoryRow>> {
    let sql = format!(
        "SELECT {} FROM memories WHERE {clause} ORDER BY {order}",
        columns()
    );
    let mut stmt = conn.prepare_cached(&sql).map_err(sqlite_error)?;
    let rows = stmt
        .query_map(params_from_iter(bind), memory_row)
        .map_err(sqlite_error)?;
    rows.collect::<rusqlite::Result<_>>().map_err(sqlite_error)
}

/// Memory `id`'s row, if there is one.
pub(super) fn stored(conn: &Connection, id: &str) -> Result<Option<MemoryRow>> {
    Ok(rows_where(conn, "id = ?1", "id", &[Sql::Text(id.to_string())])?.pop())
}

/// `clause` restricted to rows carrying every tag in `tags`, with the
/// tags appended to `bind`.
fn with_tags(clause: &str, tags: &[String], bind: &mut Vec<Sql>) -> String {
    let mut sql = clause.to_string();
    for tag in tags {
        bind.push(Sql::Text(tag.clone()));
        sql.push_str(&format!(
            " AND id IN (SELECT memory_id FROM memory_tags WHERE tag = ?{})",
            bind.len()
        ));
    }
    sql
}

/// An FTS5 query matching any of `phrases`, each as a quoted phrase.
fn fts_query(phrases: &[String]) -> String {
    phrases
        .iter()
        .map(|p| format!("\"{}\"", p.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

impl SqliteTables {
    /// Apply `edit` to memory `id` in one savepoint, keeping the full-text
    /// index, the tag table and the outbox in step, as the engine's
    /// `memories::write` does in one journal batch.
    pub(super) fn write_memory(
        &mut self,
        id: &str,
        origin: Origin,
        edit: impl FnOnce(Option<&MemoryRow>) -> Result<Edit>,
    ) -> Result<()> {
        self.atomic(|c| {
            let before = stored(c, id)?;
            let after = match edit(before.as_ref())? {
                Edit::Keep => return Ok(()),
                Edit::Delete if before.is_none() => return Ok(()),
                Edit::Delete => None,
                Edit::Put(row) => Some(*row),
            };
            if let Some(old) = &before {
                unindex(c, old)?;
            }
            match &after {
                None => c
                    .execute("DELETE FROM memories WHERE id = ?1", [id])
                    .map(drop),
                Some(row) => put(c, row),
            }
            .map_err(sqlite_error)?;
            let Some(row) = after else { return Ok(()) };
            if row.id != id {
                return Err(StoreError::Engine(format!(
                    "sqlite: a write to memory {id:?} tried to store {:?}",
                    row.id
                )));
            }
            index(c, &row)?;
            if let Some(operation) = queued_operation(before.as_ref(), &row, origin) {
                if sync_enabled(c)? {
                    queue(c, id, operation, &row.payload().to_string())?;
                }
            }
            Ok(())
        })
    }

    /// Change memory `id` if it exists, as an `UPDATE … WHERE id` would.
    pub(super) fn update_memory(
        &mut self,
        id: &str,
        origin: Origin,
        change: impl FnOnce(&mut MemoryRow) -> Result<()>,
    ) -> Result<()> {
        self.write_memory(id, origin, |before| {
            let Some(row) = before else {
                return Ok(Edit::Keep);
            };
            let mut row = row.clone();
            change(&mut row)?;
            Ok(Edit::Put(Box::new(row)))
        })
    }

    pub(super) fn rows(&self, clause: &str, order: &str, bind: &[Sql]) -> Result<Vec<MemoryRow>> {
        rows_where(&self.conn, clause, order, bind)
    }

    pub(super) fn row(&self, id: &str) -> Result<Option<MemoryRow>> {
        stored(&self.conn, id)
    }

    /// Live rows matching any of `phrases`, best BM25 first, ties by id.
    fn ranked(&self, phrases: &[String]) -> Result<Vec<(MemoryRow, f64)>> {
        let sql = format!(
            "SELECT {}, bm25(memories_fts) FROM memories_fts JOIN memories m ON m.rowid = memories_fts.rowid \
             WHERE memories_fts MATCH ?1 AND m.deleted_at IS NULL AND m.superseded_by IS NULL \
             ORDER BY bm25(memories_fts), m.id",
            MEMORY_COLUMNS.iter().map(|c| format!("m.{c}")).collect::<Vec<_>>().join(", ")
        );
        let mut stmt = self.conn.prepare_cached(&sql).map_err(sqlite_error)?;
        let rows = stmt
            .query_map([fts_query(phrases)], |r| Ok((memory_row(r)?, r.get(41)?)))
            .map_err(sqlite_error)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sqlite_error)
    }

    /// Ids of rows matching `clause`, sorted.
    fn ids(&self, clause: &str, bind: &[Sql]) -> Result<Vec<String>> {
        let sql = format!("SELECT id FROM memories WHERE {clause} ORDER BY id");
        let mut stmt = self.conn.prepare_cached(&sql).map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params_from_iter(bind), |r| r.get(0))
            .map_err(sqlite_error)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sqlite_error)
    }

    /// Ids of the memories linked to entity `entity_id`.
    pub(super) fn linked_ids(&self, entity_id: &str) -> Result<HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT memory_id FROM memory_entities WHERE entity_id = ?1")
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([entity_id], |r| r.get(0))
            .map_err(sqlite_error)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sqlite_error)
    }
}

/// Insert or overwrite `row`, keeping its rowid (the full-text key).
fn put(c: &Connection, row: &MemoryRow) -> rusqlite::Result<()> {
    let marks: Vec<String> = (1..=MEMORY_COLUMNS.len())
        .map(|i| format!("?{i}"))
        .collect();
    let set: Vec<String> = MEMORY_COLUMNS[1..]
        .iter()
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    let sql = format!(
        "INSERT INTO memories ({}) VALUES ({}) ON CONFLICT (id) DO UPDATE SET {}",
        columns(),
        marks.join(", "),
        set.join(", ")
    );
    c.prepare_cached(&sql)?
        .execute(params_from_iter(values(row)))
        .map(drop)
}

/// Put `row` into the full-text index (live rows only, as the engine) and
/// the tag table.
fn index(c: &Connection, row: &MemoryRow) -> Result<()> {
    let rowid: i64 = c
        .query_row("SELECT rowid FROM memories WHERE id = ?1", [&row.id], |r| {
            r.get(0)
        })
        .map_err(sqlite_error)?;
    if row.is_live() {
        c.prepare_cached(
            "INSERT INTO memories_fts (rowid, content, category, tags) VALUES (?1, ?2, ?3, ?4)",
        )
        .and_then(|mut s| s.execute(params![rowid, row.content, row.category, row.tags]))
        .map_err(sqlite_error)?;
    }
    let mut stmt = c
        .prepare_cached("INSERT OR IGNORE INTO memory_tags VALUES (?1, ?2)")
        .map_err(sqlite_error)?;
    for tag in row.tag_values() {
        stmt.execute(params![tag, row.id]).map_err(sqlite_error)?;
    }
    Ok(())
}

/// Take `row` out of the full-text index and the tag table.
fn unindex(c: &Connection, row: &MemoryRow) -> Result<()> {
    c.execute(
        "DELETE FROM memories_fts WHERE rowid = (SELECT rowid FROM memories WHERE id = ?1)",
        [&row.id],
    )
    .and_then(|_| c.execute("DELETE FROM memory_tags WHERE memory_id = ?1", [&row.id]))
    .map(drop)
    .map_err(sqlite_error)
}

/// Store `new` in place of `old` (the same id or another), the indexes
/// kept in step and nothing queued: a raw write for `crate::testing`.
pub(super) fn replace(c: &Connection, old: &MemoryRow, new: &MemoryRow) -> Result<()> {
    unindex(c, old)?;
    if new.id != old.id {
        c.execute("DELETE FROM memories WHERE id = ?1", [&old.id])
            .map_err(sqlite_error)?;
    }
    put(c, new).map_err(sqlite_error)?;
    index(c, new)
}

/// Rebuild the full-text index and tag table from the rows.
pub(super) fn rebuild(c: &Connection) -> Result<()> {
    c.execute_batch("DELETE FROM memories_fts; DELETE FROM memory_tags;")
        .map_err(sqlite_error)?;
    for row in rows_where(c, "1", "id", &[])? {
        index(c, &row)?;
    }
    Ok(())
}

/// Whether the outbox is on.
pub(super) fn sync_enabled(c: &Connection) -> Result<bool> {
    let value: Option<String> = c
        .query_row(
            "SELECT value FROM flags WHERE key = ?1",
            [SYNC_ENABLED],
            |r| r.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    Ok(value.as_deref() == Some("1"))
}

/// Queue an outbox entry under the next id, stamped now.
pub(super) fn queue(c: &Connection, key: &str, operation: &str, payload: &str) -> Result<()> {
    c.prepare_cached(
        "INSERT INTO sync_outbox (memory_id, operation, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
    )
    .and_then(|mut s| s.execute(params![key, operation, payload, now_iso()]))
    .map(drop)
    .map_err(sqlite_error)
}

pub(super) fn text(s: &str) -> Sql {
    Sql::Text(s.to_string())
}

impl MemoryStore for SqliteTables {
    fn insert(&mut self, new: &NewMemory) -> Result<()> {
        self.write_memory(&new.id, Origin::Local, |before| match before {
            Some(_) => Err(StoreError::Engine(format!(
                "sqlite: memory {:?} already exists",
                new.id
            ))),
            None => Ok(Edit::Put(Box::new(MemoryRow::from_new(new)))),
        })
    }

    fn insert_or_ignore(&mut self, new: &NewMemory) -> Result<bool> {
        let mut inserted = false;
        self.write_memory(&new.id, Origin::Local, |before| {
            if before.is_some() {
                return Ok(Edit::Keep);
            }
            inserted = true;
            Ok(Edit::Put(Box::new(MemoryRow::from_new(new))))
        })?;
        Ok(inserted)
    }

    fn upsert_synced(&mut self, new: &NewMemory) -> Result<()> {
        self.write_memory(&new.id, Origin::Sync, |before| {
            let mut row = MemoryRow::from_new(new);
            if let Some(local) = before {
                row.created_at.clone_from(&local.created_at);
                row.doc_id.clone_from(&local.doc_id);
                row.chunk_index = local.chunk_index;
            }
            Ok(Edit::Put(Box::new(row)))
        })
    }

    fn sync_view(&self, id: &str) -> Result<Option<SyncView>> {
        Ok(self.row(id)?.map(|row| SyncView {
            tags: serde_json::from_str(&row.tags).unwrap_or_default(),
            metadata: serde_json::from_str(&row.metadata)
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
            deleted: row.deleted_at.is_some(),
            updated_at: row.updated_at,
        }))
    }

    fn set_tags_and_metadata(&mut self, id: &str, tags: &[String], metadata: &Value) -> Result<()> {
        self.update_memory(id, Origin::Sync, |row| {
            crate::dialog::refuse_category(&row.id, &row.category)?;
            row.tags = tags_json(tags);
            row.metadata = metadata.to_string();
            Ok(())
        })
    }

    fn set_superseded_by(&mut self, id: &str, by: &str, updated_at: Option<&str>) -> Result<()> {
        self.update_memory(id, Origin::Local, |row| {
            crate::dialog::refuse_category(&row.id, &row.category)?;
            row.superseded_by = Some(by.to_string());
            if let Some(stamp) = updated_at {
                row.updated_at = stamp.to_string();
            }
            Ok(())
        })
    }

    fn supersede_import(&mut self, old: &str, new: &str, updated_at: &str) -> Result<usize> {
        // A dialog is never retired automatically (D13).
        let ids: Vec<String> = self
            .rows(
                "deleted_at IS NULL AND superseded_by IS NULL AND category != ?1",
                "id",
                &[text(DIALOG_CATEGORY)],
            )?
            .into_iter()
            .filter(|row| {
                row.metadata_value()
                    .is_some_and(|m| m.get("import_id").and_then(Value::as_str) == Some(old))
            })
            .map(|row| row.id)
            .collect();
        for id in &ids {
            self.set_superseded_by(id, new, Some(updated_at))?;
        }
        Ok(ids.len())
    }

    fn delete_capture_category(&mut self, capture_id: &str, category: &str) -> Result<usize> {
        let ids = self.ids(
            "capture_id = ?1 AND category = ?2",
            &[text(capture_id), text(category)],
        )?;
        for id in &ids {
            self.write_memory(id, Origin::Local, |_| Ok(Edit::Delete))?;
        }
        Ok(ids.len())
    }

    fn set_merged(
        &mut self,
        id: &str,
        content: &str,
        access_count: i64,
        tags: &[String],
        updated_at: &str,
    ) -> Result<()> {
        self.update_memory(id, Origin::Local, |row| {
            crate::dialog::refuse_category(&row.id, &row.category)?;
            row.content = content.to_string();
            row.access_count = access_count;
            row.tags = tags_json(tags);
            row.updated_at = updated_at.to_string();
            Ok(())
        })
    }

    fn set_vitality(&mut self, id: &str, vitality: f64, status: &str) -> Result<()> {
        self.update_memory(id, Origin::Local, |row| {
            row.vitality = vitality;
            row.status = status.to_string();
            Ok(())
        })
    }

    fn access_inputs(&self, ids: &[String]) -> Result<Vec<AccessInputs>> {
        let mut found = Vec::new();
        for id in distinct(ids) {
            if let Some(row) = self.row(id)? {
                found.push(AccessInputs {
                    id: row.id,
                    access_count: row.access_count,
                    decay_rate: row.decay_rate,
                    base_weight: row.base_weight,
                });
            }
        }
        Ok(found)
    }

    fn record_access(
        &mut self,
        id: &str,
        accessed_at: &str,
        access_count: i64,
        vitality: f64,
        status: &str,
    ) -> Result<()> {
        self.update_memory(id, Origin::Local, |row| {
            row.accessed_at = Some(accessed_at.to_string());
            row.access_count = access_count;
            row.vitality = vitality;
            row.status = status.to_string();
            Ok(())
        })
    }

    fn live_triples_except(&self, except_id: &str) -> Result<Vec<Triple>> {
        // A dialog is never a contradiction candidate (D13).
        let rows = self.rows(
            "id != ?1 AND deleted_at IS NULL AND superseded_by IS NULL AND category != ?2 \
             AND subject IS NOT NULL AND predicate IS NOT NULL AND object IS NOT NULL",
            "id",
            &[text(except_id), text(DIALOG_CATEGORY)],
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some(Triple {
                    subject: row.subject?,
                    predicate: row.predicate?,
                    object: row.object?,
                    id: row.id,
                })
            })
            .collect())
    }

    fn live_created_after(
        &self,
        cutoff: &str,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CreatedMemory>> {
        let rows = self.rows(
            "deleted_at IS NULL AND superseded_by IS NULL AND created_at >= ?1",
            "created_at, id",
            &[text(cutoff)],
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| is_after(row, cutoff, after_id))
            .take(limit)
            .map(|row| CreatedMemory {
                id: row.id,
                category: row.category,
                content: row.content,
                created_at: row.created_at,
            })
            .collect())
    }

    fn count_live_created_after(&self, cutoff: &str, after_id: Option<&str>) -> Result<usize> {
        self.conn
            .query_row(
                "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND superseded_by IS NULL \
                 AND (created_at > ?1 OR (created_at = ?1 AND ?2 IS NOT NULL AND id > ?2))",
                params![cutoff, after_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| usize::try_from(n).unwrap_or(0))
            .map_err(sqlite_error)
    }

    fn get_many(&self, ids: &[String]) -> Result<Vec<Memory>> {
        let mut found = Vec::new();
        for id in distinct(ids) {
            if let Some(row) = self.row(id)? {
                found.push(row.to_memory());
            }
        }
        Ok(found)
    }

    fn exists(&self, id: &str) -> Result<bool> {
        Ok(self.row(id)?.is_some())
    }

    fn sensitivity(&self, id: &str) -> Result<Option<bool>> {
        Ok(self.row(id)?.map(|row| row.sensitive))
    }

    fn exportable(
        &self,
        include_deleted: bool,
        category: Option<&str>,
        tags: &[String],
    ) -> Result<Vec<Memory>> {
        let mut bind = Vec::new();
        let mut clause = String::from(if include_deleted {
            "1"
        } else {
            "deleted_at IS NULL AND superseded_by IS NULL"
        });
        if let Some(c) = category {
            bind.push(text(c));
            clause.push_str(&format!(" AND category = ?{}", bind.len()));
        }
        let clause = with_tags(&clause, tags, &mut bind);
        Ok(memories(self.rows(&clause, "created_at, id", &bind)?))
    }

    fn all_live(&self) -> Result<Vec<Memory>> {
        Ok(memories(self.rows("deleted_at IS NULL", "id", &[])?))
    }

    fn live_of_type(&self, memory_type: &str) -> Result<Vec<Memory>> {
        Ok(memories(self.rows(
            "deleted_at IS NULL AND memory_type = ?1",
            "created_at, id",
            &[text(memory_type)],
        )?))
    }

    fn with_predicate_prefix(&self, prefix: &str) -> Result<Vec<Memory>> {
        let prefix = prefix.to_lowercase();
        let rows = self.rows(
            "deleted_at IS NULL AND predicate IS NOT NULL",
            "created_at, id",
            &[],
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                row.predicate
                    .as_deref()
                    .is_some_and(|p| p.trim().to_lowercase().starts_with(&prefix))
            })
            .map(|row| row.to_memory())
            .collect())
    }

    fn get_live(&self, id: &str) -> Result<Option<Memory>> {
        Ok(self
            .row(id)?
            .filter(|row| row.deleted_at.is_none())
            .map(|row| row.to_memory()))
    }

    fn list_page(
        &self,
        filter: &ListFilter,
        limit: usize,
        offset: usize,
    ) -> Result<(usize, Vec<Memory>)> {
        let mut bind = Vec::new();
        let mut clause = String::from("deleted_at IS NULL");
        if !filter.include_sensitive {
            clause.push_str(" AND sensitive = 0");
        }
        for (column, value) in [("category", &filter.category), ("source", &filter.source)] {
            if let Some(v) = value {
                bind.push(text(v));
                clause.push_str(&format!(" AND {column} = ?{}", bind.len()));
            }
        }
        let clause = with_tags(&clause, &filter.tags, &mut bind);
        let rows: Vec<MemoryRow> = self
            .rows(&clause, "created_at DESC, id DESC", &bind)?
            .into_iter()
            .filter(|row| row.in_scope(&filter.scope))
            .collect();
        Ok(page(rows, limit, offset))
    }

    fn apply_edit(&mut self, id: &str, edit: &MemoryEdit) -> Result<()> {
        self.update_memory(id, Origin::Local, |row| {
            crate::db::engine::memories::apply_edit_to(row, edit);
            Ok(())
        })
    }

    fn empty_tombstones(&mut self) -> Result<usize> {
        self.atomic(|c| {
            let mut emptied = 0;
            for mut row in rows_where(c, "deleted_at IS NOT NULL", "id", &[])? {
                if row.empty_if_tombstone() {
                    unindex(c, &row)?;
                    put(c, &row).map_err(sqlite_error)?;
                    index(c, &row)?;
                    emptied += 1;
                }
            }
            Ok(emptied)
        })
    }

    fn delete_live(&mut self, id: &str, tombstone_at: Option<&str>) -> Result<bool> {
        let mut deleted = false;
        self.write_memory(id, Origin::Local, |before| {
            let Some(row) = before.filter(|r| r.deleted_at.is_none()) else {
                return Ok(Edit::Keep);
            };
            deleted = true;
            let Some(at) = tombstone_at else {
                return Ok(Edit::Delete);
            };
            let mut row = row.clone();
            row.deleted_at = Some(at.to_string());
            row.updated_at = at.to_string();
            row.empty_if_tombstone();
            Ok(Edit::Put(Box::new(row)))
        })?;
        Ok(deleted)
    }

    fn of_type_page(
        &self,
        memory_type: &str,
        limit: usize,
    ) -> Result<(usize, Vec<UnclassifiedMemory>)> {
        // A dialog is never retyped (`crate::dialog`).
        let rows = self.rows(
            "deleted_at IS NULL AND memory_type = ?1 AND category != ?2",
            "created_at, id",
            &[text(memory_type), text(DIALOG_CATEGORY)],
        )?;
        let total = rows.len();
        let page = rows
            .into_iter()
            .take(limit)
            .map(|row| UnclassifiedMemory {
                tags: serde_json::from_str(&row.tags).unwrap_or_default(),
                content_snippet: row.content.chars().take(500).collect(),
                id: row.id,
                category: row.category,
            })
            .collect();
        Ok((total, page))
    }

    fn keyword_hits(
        &self,
        phrases: &[String],
        filter: &KeywordFilter,
        limit: usize,
    ) -> Result<Vec<(Memory, f64)>> {
        if phrases.is_empty() {
            return Ok(Vec::new());
        }
        let now = chrono::Utc::now();
        Ok(self
            .ranked(phrases)?
            .into_iter()
            .filter(|(row, _)| row.admits(filter))
            .map(|(row, score)| (row.to_memory(), score))
            .filter(|(memory, _)| filter.narrowing.admits(memory))
            .filter(|(memory, _)| filter.vital_enough(memory, now))
            .take(limit)
            .collect())
    }

    fn window_hits(&self, filter: &KeywordFilter) -> Result<Vec<Memory>> {
        let now = chrono::Utc::now();
        let field = |row: &MemoryRow| {
            crate::time_window::parse_rfc3339(filter.window.pick(&row.created_at, &row.updated_at))
        };
        let mut hits: Vec<MemoryRow> = self
            .rows("deleted_at IS NULL AND superseded_by IS NULL", "id", &[])?
            .into_iter()
            .filter(|row| row.admits(filter))
            .collect();
        hits.sort_by(|a, b| field(b).cmp(&field(a)).then_with(|| a.id.cmp(&b.id)));
        Ok(hits
            .into_iter()
            .map(|row| row.to_memory())
            .filter(|memory| filter.narrowing.admits(memory))
            .filter(|memory| filter.vital_enough(memory, now))
            .collect())
    }

    fn sensitive_ids(&self) -> Result<HashSet<String>> {
        Ok(self.ids("sensitive != 0", &[])?.into_iter().collect())
    }

    fn keyword_page(
        &self,
        phrases: &[String],
        filter: &PageFilter,
        limit: usize,
        offset: usize,
    ) -> Result<(usize, Vec<Memory>)> {
        let linked = match &filter.entity {
            Some(scope) => self.linked_ids(&scope.id)?,
            None => HashSet::new(),
        };
        let candidates: Vec<MemoryRow> = if phrases.is_empty() {
            self.rows(
                "deleted_at IS NULL AND superseded_by IS NULL",
                "created_at DESC, id DESC",
                &[],
            )?
        } else {
            self.ranked(phrases)?
                .into_iter()
                .map(|(row, _)| row)
                .collect()
        };
        let tagged: Option<HashSet<String>> = match filter.tags.is_empty() {
            true => None,
            false => {
                let mut bind = Vec::new();
                let clause = with_tags("1", &filter.tags, &mut bind);
                Some(self.ids(&clause, &bind)?.into_iter().collect())
            }
        };
        // `lower()` folds ASCII only, as SQLite's built-in does.
        let names = |row: &MemoryRow, canonical: &str| {
            let is = |v: &Option<String>| {
                v.as_ref()
                    .is_some_and(|v| v.to_ascii_lowercase() == canonical)
            };
            is(&row.subject) || is(&row.object)
        };
        let matching: Vec<MemoryRow> = candidates
            .into_iter()
            .filter(|row| filter.category.as_ref().is_none_or(|c| &row.category == c))
            .filter(|row| tagged.as_ref().is_none_or(|t| t.contains(&row.id)))
            .filter(|row| {
                filter
                    .entity
                    .as_ref()
                    .is_none_or(|scope| linked.contains(&row.id) || names(row, &scope.canonical))
            })
            .collect();
        Ok(page(matching, limit, offset))
    }

    fn unannotated_page(&self, limit: usize) -> Result<(usize, Vec<UnannotatedMemory>)> {
        let rows = self.rows(
            "deleted_at IS NULL AND superseded_by IS NULL AND category NOT IN ('dialog', 'skeleton') \
             AND subject IS NULL AND predicate IS NULL AND object IS NULL \
             AND id NOT IN (SELECT memory_id FROM memory_entities)",
            "created_at DESC, id DESC",
            &[],
        )?;
        let total = rows.len();
        let page = rows
            .into_iter()
            .take(limit)
            .map(|row| UnannotatedMemory {
                content_snippet: row.content.chars().take(500).collect(),
                tags: serde_json::from_str(&row.tags).unwrap_or_default(),
                memory_type: row.memory_type,
                id: row.id,
                category: row.category,
            })
            .collect();
        Ok((total, page))
    }

    fn with_code_refs(&self) -> Result<Vec<(String, String, String)>> {
        let rows = self.rows(
            "deleted_at IS NULL AND superseded_by IS NULL AND sensitive = 0",
            "id",
            &[],
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                row.metadata_value()
                    .is_some_and(|m| m.get("code_refs").is_some_and(|v| !v.is_null()))
            })
            .map(|row| (row.id, row.content, row.metadata))
            .collect())
    }

    fn with_valid_until(&self) -> Result<Vec<(String, String, String)>> {
        let rows = self.rows(
            "deleted_at IS NULL AND superseded_by IS NULL AND sensitive = 0 AND valid_until IS NOT NULL",
            "id",
            &[],
        )?;
        let mut found: Vec<(String, String, String)> = rows
            .into_iter()
            .filter_map(|row| row.valid_until.map(|until| (row.id, row.content, until)))
            .collect();
        found.sort();
        Ok(found)
    }

    fn capture_links(&self, id: &str) -> Result<(Vec<String>, Vec<String>)> {
        let Some(row) = self.row(id)? else {
            return Ok((Vec::new(), Vec::new()));
        };
        let ids_where = |column: &str, value: &Option<String>| -> Result<Vec<String>> {
            match value {
                None => Ok(Vec::new()),
                Some(v) => self.ids(
                    &format!("deleted_at IS NULL AND id != ?1 AND {column} = ?2"),
                    &[text(id), text(v)],
                ),
            }
        };
        Ok((
            ids_where("capture_id", &row.source_capture_id)?,
            ids_where("source_capture_id", &row.capture_id)?,
        ))
    }

    fn set_ingest_marker(&mut self, doc_id: &str, marker: &str) -> Result<usize> {
        let ids = self.ids("doc_id = ?1", &[text(doc_id)])?;
        for id in &ids {
            self.update_memory(id, Origin::Local, |row| {
                crate::db::engine::memories::stamp_ingest(row, marker)
            })?;
        }
        Ok(ids.len())
    }
}

/// `rows` as memories.
fn memories(rows: Vec<MemoryRow>) -> Vec<Memory> {
    rows.iter().map(MemoryRow::to_memory).collect()
}

/// The total, and the page of `rows` at `offset`.
fn page(rows: Vec<MemoryRow>, limit: usize, offset: usize) -> (usize, Vec<Memory>) {
    let total = rows.len();
    let page = rows
        .iter()
        .skip(offset)
        .take(limit)
        .map(MemoryRow::to_memory)
        .collect();
    (total, page)
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::memories::memory_conformance!(SqliteTables::open_in_memory().unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    const T1: &str = "2026-09-26T00:00:00+00:00";
    const T2: &str = "2026-09-27T00:00:00+00:00";

    fn hits(tables: &SqliteTables, term: &str) -> Vec<String> {
        let mut ids: Vec<String> = tables
            .ranked(&[term.to_string()])
            .unwrap()
            .into_iter()
            .map(|(row, _)| row.id)
            .collect();
        ids.sort();
        ids
    }

    fn tagged(tables: &SqliteTables, tag: &str) -> Vec<String> {
        let mut bind = Vec::new();
        let clause = with_tags("1", &[tag.to_string()], &mut bind);
        tables.ids(&clause, &bind).unwrap()
    }

    #[test]
    fn the_indexes_follow_a_memory_through_update_and_delete() {
        let mut tables = SqliteTables::open_in_memory().unwrap();
        let mut new = NewMemory::new("a", "quokka", T1);
        new.tags = vec!["red".to_string()];
        MemoryStore::insert(&mut tables, &new).unwrap();
        assert_eq!(hits(&tables, "quokka"), ["a"]);
        assert_eq!(tagged(&tables, "red"), ["a"]);

        tables
            .set_merged("a", "wombat", 2, &["blue".to_string()], T2)
            .unwrap();
        assert!(hits(&tables, "quokka").is_empty());
        assert_eq!(hits(&tables, "wombat"), ["a"]);
        assert!(tagged(&tables, "red").is_empty());
        assert_eq!(tagged(&tables, "blue"), ["a"]);

        assert!(tables.delete_live("a", Some(T2)).unwrap());
        assert!(
            hits(&tables, "wombat").is_empty(),
            "a tombstone is not searchable"
        );
        assert!(tables.delete_live("a", Some(T2)).is_ok_and(|live| !live));
    }

    #[test]
    fn the_outbox_queues_only_while_sync_is_enabled() {
        let mut tables = SqliteTables::open_in_memory().unwrap();
        let count = |t: &SqliteTables| -> i64 {
            t.conn
                .query_row("SELECT count(*) FROM sync_outbox", [], |r| r.get(0))
                .unwrap()
        };
        MemoryStore::insert(&mut tables, &NewMemory::new("a", "x", T1)).unwrap();
        assert_eq!(count(&tables), 0);
        tables
            .conn
            .execute("INSERT INTO flags VALUES ('sync_enabled', '1')", [])
            .unwrap();
        MemoryStore::insert(&mut tables, &NewMemory::new("b", "y", T1)).unwrap();
        tables.set_superseded_by("b", "a", Some(T2)).unwrap();
        tables.upsert_synced(&NewMemory::new("c", "z", T1)).unwrap();
        assert_eq!(
            count(&tables),
            2,
            "an insert and an update; a synced row is not queued"
        );
    }

    #[test]
    fn a_dialog_is_not_rewritten() {
        let mut tables = SqliteTables::open_in_memory().unwrap();
        let mut dialog = NewMemory::new("d", "verbatim", T1);
        dialog.category = DIALOG_CATEGORY.to_string();
        MemoryStore::insert(&mut tables, &dialog).unwrap();
        assert!(tables.set_merged("d", "rewritten", 0, &[], T2).is_err());
        assert_eq!(tables.get_live("d").unwrap().unwrap().content, "verbatim");
    }
}
