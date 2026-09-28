//! Storage for memory rows: every insert, sync upsert and field update of
//! `memories` that used to be written inline by a domain module.
//!
//! This is ADR-0023's phase 1, step 1. Every write goes through
//! `db::derived::write_memory`, which keeps the derived data (the FTS index,
//! the tag index and the sync outbox) in step, as triggers did up to schema
//! v30 (step 7). The rules stay with their modules: what a capture, a promotion or
//! a consolidation writes, and how vitality is seeded, are decided there and
//! handed over as a [`NewMemory`] or a field value.

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineLock};
use super::{Result, Store};
use crate::db::derived::{memory_ids, write_memory, Origin};
use crate::db::queries::{parse_memory_row, prefixed_memory_columns, MEMORY_COLUMNS};
use crate::models::{Memory, UnannotatedMemory, UnclassifiedMemory};
use crate::vitality::EFFECTIVE_VITALITY_FN;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::HashSet;

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

/// Which memories a listing takes: live ones, of `category` and `source`
/// when given, carrying every one of `tags`, and sensitive ones only when
/// asked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListFilter {
    pub include_sensitive: bool,
    pub category: Option<String>,
    pub source: Option<String>,
    pub tags: Vec<String>,
}

/// An edit to a memory's fields: each `Some` field is written, the rest keep
/// their value, and `updated_at` is always stamped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryEdit {
    pub content: Option<String>,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
    pub metadata: Option<Value>,
    pub sensitive: Option<bool>,
    /// Clear `superseded_by`. There is no way to set it here: superseding is
    /// [`Memories::set_superseded_by`]'s job.
    pub clear_superseded: bool,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub memory_type: Option<String>,
    pub decay_rate: Option<f64>,
    pub updated_at: String,
}

impl MemoryEdit {
    /// An edit that only stamps `updated_at`; set the fields to write.
    pub fn at(updated_at: impl Into<String>) -> Self {
        Self {
            updated_at: updated_at.into(),
            ..Self::default()
        }
    }

    /// The `SET` list and its bindings, in the column order the edit names.
    fn assignments(&self) -> (Vec<&'static str>, Vec<SqlValue>) {
        let mut sets = Vec::new();
        let mut bindings = Vec::new();
        let mut text = |column: &'static str, value: &Option<String>| {
            if let Some(v) = value {
                sets.push(column);
                bindings.push(SqlValue::Text(v.clone()));
            }
        };
        text("content = ?", &self.content);
        text("category = ?", &self.category);
        text("tags = ?", &self.tags.as_deref().map(tags_json));
        text(
            "metadata = ?",
            &self.metadata.as_ref().map(Value::to_string),
        );
        text("subject = ?", &self.subject);
        text("predicate = ?", &self.predicate);
        text("object = ?", &self.object);
        text("memory_type = ?", &self.memory_type);
        if let Some(sensitive) = self.sensitive {
            sets.push("sensitive = ?");
            bindings.push(SqlValue::Integer(i64::from(sensitive)));
        }
        if let Some(rate) = self.decay_rate {
            sets.push("decay_rate = ?");
            bindings.push(SqlValue::Real(rate));
        }
        if self.clear_superseded {
            sets.push("superseded_by = NULL");
        }
        sets.push("updated_at = ?");
        bindings.push(SqlValue::Text(self.updated_at.clone()));
        (sets, bindings)
    }
}

/// Which live, unsuperseded memories a ranked keyword search takes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeywordFilter {
    /// Only memories whose effective vitality, as of now, is at least this.
    pub min_effective_vitality: Option<f64>,
    pub category: Option<String>,
    pub include_sensitive: bool,
}

/// Which live, unsuperseded memories a paged search takes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageFilter {
    pub category: Option<String>,
    /// All-of.
    pub tags: Vec<String>,
    /// Only memories linked to this entity, or whose subject or object is its
    /// canonical name.
    pub entity: Option<EntityScope>,
}

/// An entity a paged search is narrowed to.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityScope {
    pub id: String,
    /// The entity's normalized name, compared with `lower(subject)` and
    /// `lower(object)`.
    pub canonical: String,
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
pub(crate) fn tags_json(tags: &[String]) -> String {
    serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string())
}

/// The `memories` table, over one connection, or on the engine's memories
/// core when the store's tables hold it (`db::engine::memories`).
pub struct Memories<'c> {
    conn: &'c Connection,
    #[cfg(feature = "engine-store")]
    core: Option<&'c EngineLock>,
}

/// Answer from the engine's memories core when the repository has it.
macro_rules! on_core {
    ($self:ident, |$tables:ident| $body:expr) => {
        #[cfg(feature = "engine-store")]
        if let Some(core) = $self.core {
            #[allow(unused_mut)]
            let mut $tables = core.lock();
            return $body;
        }
    };
}

impl<'c> Memories<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            #[cfg(feature = "engine-store")]
            core: store.core(),
        }
    }

    /// Insert `row`, made on this node. An existing id is an error.
    pub fn insert(&self, row: &NewMemory) -> Result<()> {
        on_core!(self, |tables| engine::memories::insert(&mut tables, row));
        write_memory(self.conn, &row.id, Origin::Local, || {
            Ok(self.conn.execute(
                &format!("INSERT INTO memories ({INSERT_COLUMNS}) VALUES ({INSERT_PLACEHOLDERS})"),
                params_from_iter(insert_values(row)),
            )?)
        })?;
        Ok(())
    }

    /// Insert `row` unless its id is already taken. Returns whether it was
    /// inserted.
    pub fn insert_or_ignore(&self, row: &NewMemory) -> Result<bool> {
        on_core!(self, |tables| engine::memories::insert_or_ignore(
            &mut tables,
            row
        ));
        let inserted = write_memory(self.conn, &row.id, Origin::Local, || {
            Ok(self.conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO memories ({INSERT_COLUMNS}) VALUES ({INSERT_PLACEHOLDERS})"
                ),
                params_from_iter(insert_values(row)),
            )?)
        })?;
        Ok(inserted > 0)
    }

    /// Write a record a peer sent: insert it, or overwrite the local row.
    ///
    /// An overwrite keeps the local `created_at`, `doc_id` and `chunk_index`:
    /// a peer's record does not carry the last two, and the first never
    /// changes. Never queued for sync: it came from there.
    pub fn upsert_synced(&self, row: &NewMemory) -> Result<()> {
        on_core!(self, |tables| engine::memories::upsert_synced(
            &mut tables,
            row
        ));
        write_memory(self.conn, &row.id, Origin::Sync, || self.upsert_row(row))?;
        Ok(())
    }

    fn upsert_row(&self, row: &NewMemory) -> Result<usize> {
        Ok(self.conn.execute(
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
        )?)
    }

    /// The local copy of `id` as a sync merge sees it, if there is one.
    /// Unparseable `tags` read as none and unparseable `metadata` as `{}`.
    pub fn sync_view(&self, id: &str) -> Result<Option<SyncView>> {
        on_core!(self, |tables| engine::memories::sync_view(&tables, id));
        Ok(self
            .conn
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
            .optional()?)
    }

    /// Replace `id`'s tags and metadata without stamping `updated_at`: a sync
    /// merge that lost last-write-wins still keeps the union, and must not
    /// look like a newer local edit.
    pub fn set_tags_and_metadata(&self, id: &str, tags: &[String], metadata: &Value) -> Result<()> {
        on_core!(self, |tables| engine::memories::set_tags_and_metadata(
            &mut tables,
            id,
            tags,
            metadata
        ));
        write_memory(self.conn, id, Origin::Sync, || {
            Ok(self.conn.execute(
                "UPDATE memories SET tags = ?, metadata = ? WHERE id = ?",
                params![tags_json(tags), metadata.to_string(), id],
            )?)
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
        on_core!(self, |tables| engine::memories::set_superseded_by(
            &mut tables,
            id,
            superseded_by,
            updated_at
        ));
        write_memory(self.conn, id, Origin::Local, || match updated_at {
            Some(stamp) => Ok(self.conn.execute(
                "UPDATE memories SET superseded_by = ?, updated_at = ? WHERE id = ?",
                params![superseded_by, stamp, id],
            )?),
            None => Ok(self.conn.execute(
                "UPDATE memories SET superseded_by = ? WHERE id = ?",
                params![superseded_by, id],
            )?),
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
        on_core!(self, |tables| engine::memories::supersede_import(
            &mut tables,
            old_import_id,
            new_import_id,
            updated_at
        ));
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
        on_core!(self, |tables| engine::memories::delete_capture_category(
            &mut tables,
            capture_id,
            category
        ));
        let ids = memory_ids(
            self.conn,
            "SELECT id FROM memories WHERE capture_id = ? AND category = ?",
            params![capture_id, category],
        )?;
        for id in &ids {
            write_memory(self.conn, id, Origin::Local, || {
                Ok(self
                    .conn
                    .execute("DELETE FROM memories WHERE id = ?", params![id])?)
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
        on_core!(self, |tables| engine::memories::set_merged(
            &mut tables,
            id,
            content,
            access_count,
            tags,
            updated_at
        ));
        write_memory(self.conn, id, Origin::Local, || {
            Ok(self.conn.execute(
                "UPDATE memories SET content = ?, access_count = ?, tags = ?, updated_at = ? WHERE id = ?",
                params![content, access_count, tags_json(tags), updated_at, id],
            )?)
        })?;
        Ok(())
    }

    /// Set `id`'s vitality and status, without stamping `updated_at`: both
    /// are local scores, not edits.
    pub fn set_vitality(&self, id: &str, vitality: f64, status: &str) -> Result<()> {
        on_core!(self, |tables| engine::memories::set_vitality(
            &mut tables,
            id,
            vitality,
            status
        ));
        write_memory(self.conn, id, Origin::Local, || {
            Ok(self.conn.execute(
                "UPDATE memories SET vitality = ?, status = ? WHERE id = ?",
                params![vitality, status, id],
            )?)
        })?;
        Ok(())
    }

    /// The access-tracking inputs of each of `ids` that exists, in no
    /// particular order.
    pub fn access_inputs(&self, ids: &[String]) -> Result<Vec<AccessInputs>> {
        on_core!(self, |tables| engine::memories::access_inputs(&tables, ids));
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
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
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
        on_core!(self, |tables| engine::memories::record_access(
            &mut tables,
            id,
            accessed_at,
            access_count,
            vitality,
            status
        ));
        // Cached: a search records an access for every result it returns.
        write_memory(self.conn, id, Origin::Local, || {
            Ok(self.conn
                .prepare_cached(
                    "UPDATE memories SET accessed_at = ?, access_count = ?, vitality = ?, status = ?
                      WHERE id = ?",
                )?
                .execute(params![accessed_at, access_count, vitality, status, id])?)
        })?;
        Ok(())
    }

    /// The triple of every live, unsuperseded memory other than `except_id`
    /// that has all three parts.
    pub fn live_triples_except(&self, except_id: &str) -> Result<Vec<Triple>> {
        on_core!(self, |tables| engine::memories::live_triples_except(
            &tables, except_id
        ));
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
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Live, unsuperseded memories created after `cutoff`, oldest first, at
    /// most `limit`.
    pub fn live_created_after(&self, cutoff: &str, limit: usize) -> Result<Vec<CreatedMemory>> {
        on_core!(self, |tables| engine::memories::live_created_after(
            &tables, cutoff, limit
        ));
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
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// How many live, unsuperseded memories were created after `cutoff`.
    pub fn count_live_created_after(&self, cutoff: &str) -> Result<usize> {
        on_core!(self, |tables| engine::memories::count_live_created_after(
            &tables, cutoff
        ));
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
        on_core!(self, |tables| engine::memories::get_many(&tables, ids));
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; ids.len()].join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {MEMORY_COLUMNS} FROM memories WHERE id IN ({marks})"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(ids.iter()), parse_memory_row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Whether `id` exists.
    pub fn exists(&self, id: &str) -> Result<bool> {
        on_core!(self, |tables| engine::memories::exists(&tables, id));
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
        on_core!(self, |tables| engine::memories::sensitivity(&tables, id));
        Ok(self
            .conn
            .query_row(
                "SELECT sensitive FROM memories WHERE id = ?",
                params![id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The memories an export takes, oldest first (ties by id): of
    /// `category` when given, carrying every one of `tags`, and only the live,
    /// unsuperseded ones unless `include_deleted`.
    pub fn exportable(
        &self,
        include_deleted: bool,
        category: Option<&str>,
        tags: &[String],
    ) -> Result<Vec<Memory>> {
        on_core!(self, |tables| engine::memories::exportable(
            &tables,
            include_deleted,
            category,
            tags
        ));
        let mut conditions: Vec<&str> = Vec::new();
        let mut bindings: Vec<SqlValue> = Vec::new();
        if !include_deleted {
            conditions.push("m.deleted_at IS NULL");
            conditions.push("m.superseded_by IS NULL");
        }
        if let Some(category) = category {
            conditions.push("m.category = ?");
            bindings.push(SqlValue::Text(category.to_string()));
        }
        // ALL-of tag semantics, against the tag index.
        for tag in tags {
            conditions.push(
                "EXISTS (SELECT 1 FROM memory_tags mt WHERE mt.memory_id = m.id AND mt.tag = ?)",
            );
            bindings.push(SqlValue::Text(tag.clone()));
        }
        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM memories m {where_clause} ORDER BY m.created_at, m.id",
            crate::db::queries::prefixed_memory_columns("m")
        ))?;
        let rows = stmt
            .query_map(params_from_iter(bindings), parse_memory_row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Every live memory, in no particular order.
    pub fn all_live(&self) -> Result<Vec<Memory>> {
        on_core!(self, |tables| engine::memories::all_live(&tables));
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {MEMORY_COLUMNS} FROM memories WHERE deleted_at IS NULL"
        ))?;
        let rows = stmt
            .query_map([], parse_memory_row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Memory `id`, unless it is missing or deleted.
    pub fn get_live(&self, id: &str) -> Result<Option<Memory>> {
        on_core!(self, |tables| engine::memories::get_live(&tables, id));
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {MEMORY_COLUMNS} FROM memories WHERE id = ? AND deleted_at IS NULL"
                ),
                params![id],
                parse_memory_row,
            )
            .optional()?)
    }

    /// Live memory `id`'s category, or `None` when it is missing or deleted.
    pub fn live_category(&self, id: &str) -> Result<Option<String>> {
        on_core!(self, |tables| Ok(
            engine::memories::get_live(&tables, id)?.map(|m| m.category)
        ));
        Ok(self
            .conn
            .query_row(
                "SELECT category FROM memories WHERE id = ? AND deleted_at IS NULL",
                params![id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The memories `filter` takes, newest first (ties by id, descending):
    /// how many there are in all, and the page at `offset` of at most
    /// `limit`.
    pub fn list_page(
        &self,
        filter: &ListFilter,
        limit: usize,
        offset: usize,
    ) -> Result<(usize, Vec<Memory>)> {
        on_core!(self, |tables| engine::memories::list_page(
            &tables, filter, limit, offset
        ));
        let mut conditions = vec!["m.deleted_at IS NULL"];
        let mut bindings: Vec<SqlValue> = Vec::new();
        if !filter.include_sensitive {
            conditions.push("m.sensitive = 0");
        }
        if let Some(category) = &filter.category {
            conditions.push("m.category = ?");
            bindings.push(SqlValue::Text(category.clone()));
        }
        if let Some(source) = &filter.source {
            conditions.push("m.source = ?");
            bindings.push(SqlValue::Text(source.clone()));
        }
        // ALL-of tag semantics, against the tag index.
        for tag in &filter.tags {
            conditions.push(
                "EXISTS (SELECT 1 FROM memory_tags mt WHERE mt.memory_id = m.id AND mt.tag = ?)",
            );
            bindings.push(SqlValue::Text(tag.clone()));
        }
        let where_clause = conditions.join(" AND ");
        let total: i64 = self.conn.query_row(
            &format!("SELECT count(*) FROM memories m WHERE {where_clause}"),
            params_from_iter(bindings.iter()),
            |r| r.get(0),
        )?;
        bindings.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        bindings.push(SqlValue::Integer(i64::try_from(offset).unwrap_or(i64::MAX)));
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {MEMORY_COLUMNS} FROM memories m WHERE {where_clause}
              ORDER BY m.created_at DESC, m.id DESC LIMIT ? OFFSET ?"
        ))?;
        let page = stmt
            .query_map(params_from_iter(bindings.iter()), parse_memory_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok((usize::try_from(total).unwrap_or(0), page))
    }

    /// Apply `edit` to memory `id`, made on this node. A missing id is a
    /// no-op.
    pub fn apply_edit(&self, id: &str, edit: &MemoryEdit) -> Result<()> {
        on_core!(self, |tables| engine::memories::apply_edit(
            &mut tables,
            id,
            edit
        ));
        let (sets, mut bindings) = edit.assignments();
        bindings.push(SqlValue::Text(id.to_string()));
        write_memory(self.conn, id, Origin::Local, || {
            Ok(self.conn.execute(
                &format!("UPDATE memories SET {} WHERE id = ?", sets.join(", ")),
                params_from_iter(bindings.iter()),
            )?)
        })?;
        Ok(())
    }

    /// Delete live memory `id`: tombstone it at `tombstone_at` (stamping
    /// `deleted_at` and `updated_at`), or remove the row when that is
    /// `None`. Whether there was a live memory to delete.
    pub fn delete_live(&self, id: &str, tombstone_at: Option<&str>) -> Result<bool> {
        on_core!(self, |tables| engine::memories::delete_live(
            &mut tables,
            id,
            tombstone_at
        ));
        let affected = write_memory(self.conn, id, Origin::Local, || {
            match tombstone_at {
            Some(at) => Ok(self.conn.execute(
                "UPDATE memories SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
                params![at, at, id],
            )?),
            None => Ok(self.conn.execute(
                "DELETE FROM memories WHERE id = ? AND deleted_at IS NULL",
                params![id],
            )?),
        }
        })?;
        Ok(affected > 0)
    }

    /// Live memories of `memory_type`, oldest first (ties by id): how many
    /// there are, and the first `limit` with the first 500 characters of
    /// their content.
    pub fn of_type_page(
        &self,
        memory_type: &str,
        limit: usize,
    ) -> Result<(usize, Vec<UnclassifiedMemory>)> {
        on_core!(self, |tables| engine::memories::of_type_page(
            &tables,
            memory_type,
            limit
        ));
        let total: i64 = self.conn.query_row(
            "SELECT count(*) FROM memories WHERE memory_type = ? AND deleted_at IS NULL",
            params![memory_type],
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare(
            "SELECT id, substr(content, 1, 500), category, tags
               FROM memories
              WHERE memory_type = ? AND deleted_at IS NULL
              ORDER BY created_at, id
              LIMIT ?",
        )?;
        let page = stmt
            .query_map(
                params![memory_type, i64::try_from(limit).unwrap_or(i64::MAX)],
                |r| {
                    let tags: String = r.get(3)?;
                    Ok(UnclassifiedMemory {
                        id: r.get(0)?,
                        content_snippet: r.get(1)?,
                        category: r.get(2)?,
                        tags: serde_json::from_str(&tags).unwrap_or_default(),
                    })
                },
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok((usize::try_from(total).unwrap_or(0), page))
    }

    /// Live, unsuperseded memories matching any of `phrases` in the full-text
    /// index, best BM25 first (ties by id), at most `limit`, each with its
    /// BM25 score. No phrases match nothing.
    pub fn keyword_hits(
        &self,
        phrases: &[String],
        filter: &KeywordFilter,
        limit: usize,
    ) -> Result<Vec<(Memory, f64)>> {
        if phrases.is_empty() {
            return Ok(Vec::new());
        }
        on_core!(self, |tables| engine::memories::keyword_hits(
            &tables, phrases, filter, limit
        ));
        let mut sql = format!(
            "SELECT {}, bm25(memories_fts) AS bm25_score
               FROM memories_fts fts
               JOIN memories m ON m.rowid = fts.rowid
              WHERE memories_fts MATCH ? AND m.superseded_by IS NULL AND m.deleted_at IS NULL",
            prefixed_memory_columns("m")
        );
        let mut bindings = vec![SqlValue::Text(crate::fts::match_expression(phrases))];
        if let Some(floor) = filter.min_effective_vitality {
            sql.push_str(&format!(
                " AND {EFFECTIVE_VITALITY_FN}(m.base_weight, m.access_count, m.decay_rate, \
                 coalesce(m.accessed_at, m.created_at)) >= ?"
            ));
            bindings.push(SqlValue::Real(floor));
        }
        if let Some(category) = &filter.category {
            sql.push_str(" AND m.category = ?");
            bindings.push(SqlValue::Text(category.clone()));
        }
        if !filter.include_sensitive {
            sql.push_str(" AND m.sensitive = 0");
        }
        sql.push_str(" ORDER BY bm25(memories_fts), m.id LIMIT ?");
        bindings.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        let mut stmt = self.conn.prepare(&sql)?;
        let hits = stmt
            .query_map(params_from_iter(bindings.iter()), |row| {
                Ok((parse_memory_row(row)?, row.get("bm25_score")?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(hits)
    }

    /// The ids of every memory marked sensitive.
    pub fn sensitive_ids(&self) -> Result<HashSet<String>> {
        on_core!(self, |tables| engine::memories::sensitive_ids(&tables));
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM memories WHERE sensitive = 1")?;
        let ids = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// A page of the live, unsuperseded memories `filter` takes: matching
    /// any of `phrases`, best BM25 first, or with no phrases newest first
    /// (ties by id either way). How many there are, and the page.
    pub fn keyword_page(
        &self,
        phrases: &[String],
        filter: &PageFilter,
        limit: usize,
        offset: usize,
    ) -> Result<(usize, Vec<Memory>)> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            let tables = core.lock();
            let linked = match &filter.entity {
                Some(scope) => engine::graph::linked_ids(&tables, &scope.id)?,
                None => HashSet::new(),
            };
            return engine::memories::keyword_page(
                &tables, phrases, filter, &linked, limit, offset,
            );
        }
        let mut conditions = String::from("m.superseded_by IS NULL AND m.deleted_at IS NULL");
        let mut bindings: Vec<SqlValue> = Vec::new();
        let mut from = String::from("memories m");
        let mut order = "m.created_at DESC, m.id DESC";
        if !phrases.is_empty() {
            from.push_str(" JOIN memories_fts fts ON m.rowid = fts.rowid");
            conditions.push_str(" AND memories_fts MATCH ?");
            bindings.push(SqlValue::Text(crate::fts::match_expression(phrases)));
            order = "bm25(memories_fts), m.id";
        }
        if let Some(category) = &filter.category {
            conditions.push_str(" AND m.category = ?");
            bindings.push(SqlValue::Text(category.clone()));
        }
        for tag in &filter.tags {
            conditions.push_str(
                " AND EXISTS (SELECT 1 FROM memory_tags mt WHERE mt.memory_id = m.id AND mt.tag = ?)",
            );
            bindings.push(SqlValue::Text(tag.clone()));
        }
        if let Some(scope) = &filter.entity {
            conditions.push_str(
                " AND (EXISTS (SELECT 1 FROM memory_entities me \
                   WHERE me.memory_id = m.id AND me.entity_id = ?) \
                   OR lower(m.subject) = ? OR lower(m.object) = ?)",
            );
            bindings.push(SqlValue::Text(scope.id.clone()));
            bindings.push(SqlValue::Text(scope.canonical.clone()));
            bindings.push(SqlValue::Text(scope.canonical.clone()));
        }
        let total: i64 = self.conn.query_row(
            &format!("SELECT count(*) FROM {from} WHERE {conditions}"),
            params_from_iter(bindings.iter()),
            |r| r.get(0),
        )?;
        bindings.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        bindings.push(SqlValue::Integer(i64::try_from(offset).unwrap_or(i64::MAX)));
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM {from} WHERE {conditions} ORDER BY {order} LIMIT ? OFFSET ?",
            prefixed_memory_columns("m")
        ))?;
        let page = stmt
            .query_map(params_from_iter(bindings.iter()), parse_memory_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok((usize::try_from(total).unwrap_or(0), page))
    }

    /// Memories awaiting extraction, newest first (ties by id, descending):
    /// how many there are, and the first `limit` with the first 500
    /// characters of their content. See [`unannotated_where`].
    pub fn unannotated_page(&self, limit: usize) -> Result<(usize, Vec<UnannotatedMemory>)> {
        on_core!(self, |tables| engine::memories::unannotated_page(
            &tables, limit
        ));
        let predicate = unannotated_where();
        let total: i64 = self.conn.query_row(
            &format!("SELECT count(*) FROM memories m WHERE {predicate}"),
            [],
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id, m.content, m.category, m.memory_type, m.tags
               FROM memories m
              WHERE {predicate}
              ORDER BY m.created_at DESC, m.id DESC
              LIMIT ?"
        ))?;
        let page = stmt
            .query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                let content: String = row.get("content")?;
                let tags_json: String = row.get("tags")?;
                Ok(UnannotatedMemory {
                    id: row.get("id")?,
                    // By characters, not bytes: a multi-byte character on the
                    // boundary would panic a byte slice.
                    content_snippet: content.chars().take(500).collect(),
                    category: row.get("category")?,
                    memory_type: row.get("memory_type")?,
                    tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok((usize::try_from(total).unwrap_or(0), page))
    }

    /// The id, content and metadata JSON of every live, unsuperseded,
    /// non-sensitive memory that records code references.
    pub fn with_code_refs(&self) -> Result<Vec<(String, String, String)>> {
        on_core!(self, |tables| engine::memories::with_code_refs(&tables));
        let mut stmt = self.conn.prepare(
            "SELECT id, content, metadata
               FROM memories
              WHERE deleted_at IS NULL
                AND superseded_by IS NULL
                AND sensitive = 0
                AND json_extract(metadata, '$.code_refs') IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Set `metadata.ingest` to `marker` on every chunk of the import
    /// `doc_id`. Returns how many were stamped.
    pub fn set_ingest_marker(&self, doc_id: &str, marker: &str) -> Result<usize> {
        on_core!(self, |tables| engine::memories::set_ingest_marker(
            &mut tables,
            doc_id,
            marker
        ));
        let ids = memory_ids(
            self.conn,
            "SELECT id FROM memories WHERE doc_id = ?",
            params![doc_id],
        )?;
        for id in &ids {
            write_memory(self.conn, id, Origin::Local, || {
                Ok(self.conn.execute(
                    "UPDATE memories SET metadata = json_set(metadata, '$.ingest', ?) WHERE id = ?",
                    params![marker, id],
                )?)
            })?;
        }
        Ok(ids.len())
    }
}

/// Which memories still need a triple or entity mentions.
///
/// A memory qualifies when it is live, is **not a raw dialog**, has no SPO
/// triple at all, and has no entity links at all.
///
/// Two parts of that are easy to get wrong. The `dialog` exclusion is not
/// cosmetic: a captured transcript's facts are meant to come out through
/// `decompose`, so without it every captured conversation would flood this
/// backlog. And a memory needs to be missing *both* signals — one that has
/// entities but no triple is already considered annotated, so an `OR` here
/// would keep re-offering work that is done.
///
/// `skeleton` is excluded on the same grounds and then some (#207): it is
/// Mermaid source describing a conversation's shape, so there is no fact in it
/// to extract and offering one costs a model call to find that out.
pub(crate) fn unannotated_where() -> &'static str {
    "m.superseded_by IS NULL
     AND m.deleted_at IS NULL
     AND m.category NOT IN ('dialog', 'skeleton')
     AND m.subject IS NULL AND m.predicate IS NULL AND m.object IS NULL
     AND NOT EXISTS (SELECT 1 FROM memory_entities me WHERE me.memory_id = m.id)"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::outbox::Outbox;
    use crate::db::sync_state::SyncState;
    use crate::db::{on_each_backend, Database};

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
        // The schema's defaults are SQLite's; the engine has no schema.
        let db = Database::open_sqlite_in_memory().unwrap();
        let store = db.store();
        let conn = store.conn();
        conn.execute(
            "INSERT INTO memories (id, content, created_at, updated_at) VALUES ('a', 'x', ?, ?)",
            params![NOW, NOW],
        )
        .unwrap();
        Memories::new(&store)
            .insert(&NewMemory::new("b", "x", NOW))
            .unwrap();

        let mut defaulted = raw_row(conn, "a");
        let mut written = raw_row(conn, "b");
        defaulted.remove(0);
        written.remove(0);
        assert_eq!(written, defaulted);
    }

    /// The one memory `id`, which must exist.
    fn get(memories: &Memories<'_>, id: &str) -> Memory {
        let mut found = memories.get_many(&[id.to_string()]).unwrap();
        assert_eq!(found.len(), 1, "memory {id}");
        found.remove(0)
    }

    #[test]
    fn insert_refuses_a_taken_id_and_insert_or_ignore_reports_it() {
        on_each_backend(|db| {
            let store = db.store();
            let memories = Memories::new(&store);
            let row = NewMemory::new("a", "first", NOW);

            assert!(memories.insert_or_ignore(&row).unwrap());
            assert!(memories.insert(&row).is_err());
            let second = NewMemory::new("a", "second", NOW);
            assert!(!memories.insert_or_ignore(&second).unwrap());
            assert_eq!(get(&memories, "a").content, "first");
        });
    }

    #[test]
    fn a_synced_overwrite_keeps_created_at_and_the_chunk_position() {
        on_each_backend(|db| {
            let store = db.store();
            let memories = Memories::new(&store);
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

            let a = get(&memories, "a");
            assert_eq!(a.content, "remote");
            assert_eq!(a.created_at, NOW);
            assert_eq!(a.doc_id.as_deref(), Some("imp_1"));
            assert_eq!(a.chunk_index, Some(3));
            assert_eq!(a.tags, ["peer"]);
        });
    }

    #[test]
    fn sync_view_reads_tags_that_are_not_an_array_as_none() {
        on_each_backend(|db| {
            let store = db.store();
            Memories::new(&store)
                .insert(&NewMemory::new("a", "x", NOW))
                .unwrap();
            crate::testing::set_memory_column(&store, "a", "tags", r#""not an array""#).unwrap();
            let view = Memories::new(&store).sync_view("a").unwrap().unwrap();
            assert!(view.tags.is_empty());
            assert_eq!(view.metadata, serde_json::json!({}));
            assert!(Memories::new(&store)
                .sync_view("missing")
                .unwrap()
                .is_none());
        });
    }

    #[test]
    fn access_inputs_skips_unknown_ids_and_takes_an_empty_list() {
        on_each_backend(|db| {
            let store = db.store();
            let memories = Memories::new(&store);
            memories.insert(&NewMemory::new("a", "x", NOW)).unwrap();

            assert!(memories.access_inputs(&[]).unwrap().is_empty());
            let found = memories
                .access_inputs(&["a".to_string(), "missing".to_string(), "a".to_string()])
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
        });
    }

    /// Everything a backend answers after [`exercise`]'s writes, in a form
    /// two backends can be compared in.
    #[derive(Debug, PartialEq)]
    struct Observed {
        counts: Vec<usize>,
        exported: Vec<Value>,
        live: Vec<Value>,
        red_live: Vec<String>,
        code_refs: Vec<(String, String, String)>,
        triples: Vec<Triple>,
        created: Vec<CreatedMemory>,
        created_count: usize,
        sensitivity: Vec<Option<bool>>,
        views: Vec<Option<SyncView>>,
        outbox: Vec<(String, Value)>,
    }

    fn as_values(mut memories: Vec<Memory>) -> Vec<Value> {
        memories.sort_by(|a, b| a.id.cmp(&b.id));
        memories
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect()
    }

    /// Every write the repository makes, with sync on, then every read.
    fn exercise(db: &Database) -> Observed {
        const T2: &str = "2026-09-27T00:00:00+00:00";
        const T3: &str = "2026-09-28T00:00:00+00:00";
        let store = db.store();
        SyncState::new(&store)
            .set_flag("sync_enabled", "1")
            .unwrap();
        let m = Memories::new(&store);
        let text = |s: &str| Some(s.to_string());
        let mut counts = Vec::new();

        m.insert(&NewMemory {
            category: "fact".into(),
            tags: vec!["red".into(), "blue".into()],
            capture_id: text("cap"),
            subject: text("sky"),
            predicate: text("is"),
            object: text("blue"),
            ..NewMemory::new("a", "alpha quokka", NOW)
        })
        .unwrap();
        counts.push(usize::from(
            m.insert_or_ignore(&NewMemory {
                tags: vec!["red".into()],
                metadata: serde_json::json!({"code_refs": ["src/x.rs"]}),
                ..NewMemory::new("b", "beta", T2)
            })
            .unwrap(),
        ));
        counts.push(usize::from(
            m.insert_or_ignore(&NewMemory::new("b", "ignored", T2))
                .unwrap(),
        ));
        m.upsert_synced(&NewMemory {
            sensitive: true,
            ..NewMemory::new("c", "gamma", T2)
        })
        .unwrap();
        for (id, chunk) in [("d1", 0), ("d2", 1)] {
            m.insert(&NewMemory {
                metadata: serde_json::json!({"import_id": "imp1"}),
                doc_id: text("doc1"),
                chunk_index: Some(chunk),
                capture_id: text("cap"),
                category: "chunk".into(),
                ..NewMemory::new(id, "chunk text", T2)
            })
            .unwrap();
        }
        m.set_tags_and_metadata("c", &["green".into()], &serde_json::json!({"k": "v"}))
            .unwrap();
        m.set_superseded_by("b", "c", None).unwrap();
        m.set_merged("c", "gamma merged", 5, &["green".into(), "red".into()], T3)
            .unwrap();
        m.set_vitality("a", 0.25, "fading").unwrap();
        m.record_access("a", T3, 3, 0.75, "active").unwrap();
        counts.push(m.set_ingest_marker("doc1", "done").unwrap());
        counts.push(m.supersede_import("imp1", "imp2", T3).unwrap());
        m.upsert_synced(&NewMemory {
            created_at: T3.into(),
            ..NewMemory::new("a", "alpha remote", T3)
        })
        .unwrap();
        m.insert(&NewMemory {
            capture_id: text("cap"),
            category: "chunk".into(),
            ..NewMemory::new("e", "doomed", T3)
        })
        .unwrap();
        counts.push(m.delete_capture_category("cap", "chunk").unwrap());

        let ids: Vec<String> = ["a", "b", "c", "d1", "e", "missing"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut triples = m.live_triples_except("none").unwrap();
        triples.sort_by(|a, b| a.id.cmp(&b.id));
        let mut code_refs = m.with_code_refs().unwrap();
        code_refs.sort();
        let outbox = Outbox::new(&store)
            .unsent_to("hub", 0, 100)
            .unwrap()
            .into_iter()
            .map(|e| (e.key, serde_json::from_str(&e.payload_json).unwrap()))
            .collect();
        Observed {
            counts,
            exported: m
                .exportable(true, None, &[])
                .unwrap()
                .iter()
                .map(|m| serde_json::to_value(m).unwrap())
                .collect(),
            live: as_values(m.all_live().unwrap()),
            red_live: m
                .exportable(false, None, &["red".into()])
                .unwrap()
                .into_iter()
                .map(|m| m.id)
                .collect(),
            code_refs,
            triples,
            created: m.live_created_after(NOW, 10).unwrap(),
            created_count: m.count_live_created_after(NOW).unwrap(),
            sensitivity: ids.iter().map(|id| m.sensitivity(id).unwrap()).collect(),
            views: ids.iter().map(|id| m.sync_view(id).unwrap()).collect(),
            outbox,
        }
    }

    /// Lists, edits and deletes on `db`, and what each read returns after.
    fn exercise_reads(db: &Database) -> Vec<Value> {
        const T2: &str = "2026-09-27T00:00:00+00:00";
        let store = db.store();
        let m = Memories::new(&store);
        let text = |s: &str| Some(s.to_string());
        for (id, at, category, tags, sensitive) in [
            ("a", NOW, "fact", vec!["red"], false),
            ("b", T2, "fact", vec!["red", "blue"], false),
            ("c", T2, "note", vec![], true),
            ("d", NOW, "note", vec!["blue"], false),
            ("e", T2, "fact", vec![], false),
        ] {
            m.insert(&NewMemory {
                category: category.into(),
                tags: tags.into_iter().map(String::from).collect(),
                sensitive,
                source: if id == "d" {
                    "import".into()
                } else {
                    "manual".into()
                },
                ..NewMemory::new(id, format!("content of {id} {}", "é".repeat(600)), at)
            })
            .unwrap();
        }
        let mut seen = Vec::new();
        let page = |filter: ListFilter, limit, offset| {
            let (total, memories) = m.list_page(&filter, limit, offset).unwrap();
            let ids: Vec<String> = memories.into_iter().map(|m| m.id).collect();
            serde_json::json!([total, ids])
        };
        seen.push(page(ListFilter::default(), 10, 0));
        seen.push(page(ListFilter::default(), 2, 1));
        seen.push(page(
            ListFilter {
                include_sensitive: true,
                ..ListFilter::default()
            },
            10,
            0,
        ));
        seen.push(page(
            ListFilter {
                category: text("fact"),
                tags: vec!["red".into()],
                ..ListFilter::default()
            },
            10,
            0,
        ));
        seen.push(page(
            ListFilter {
                source: text("import"),
                ..ListFilter::default()
            },
            10,
            0,
        ));

        m.apply_edit(
            "a",
            &MemoryEdit {
                content: text("edited"),
                tags: Some(vec!["green".into()]),
                metadata: Some(serde_json::json!({"k": 1})),
                sensitive: Some(true),
                subject: text("s"),
                memory_type: text("decision"),
                decay_rate: Some(0.02),
                ..MemoryEdit::at(T2)
            },
        )
        .unwrap();
        m.set_superseded_by("b", "a", None).unwrap();
        m.apply_edit(
            "b",
            &MemoryEdit {
                clear_superseded: true,
                ..MemoryEdit::at(T2)
            },
        )
        .unwrap();
        m.apply_edit("missing", &MemoryEdit::at(T2)).unwrap();
        seen.push(serde_json::json!([
            m.delete_live("c", Some(T2)).unwrap(),
            m.delete_live("c", Some(T2)).unwrap(),
            m.delete_live("d", None).unwrap(),
            m.delete_live("missing", None).unwrap(),
        ]));
        for id in ["a", "b", "c", "d"] {
            seen.push(serde_json::to_value(m.get_live(id).unwrap()).unwrap());
            seen.push(serde_json::to_value(m.live_category(id).unwrap()).unwrap());
        }
        let (total, page) = m.of_type_page("unclassified", 2).unwrap();
        seen.push(serde_json::json!([
            total,
            serde_json::to_value(page).unwrap()
        ]));
        seen.push(serde_json::to_value(as_values(m.exportable(true, None, &[]).unwrap())).unwrap());
        seen
    }

    #[test]
    fn the_engine_core_matches_sqlite_read_for_read() {
        let mut observed = Vec::new();
        on_each_backend(|db| observed.push(exercise_reads(db)));
        let sqlite = &observed[0];
        assert_eq!(sqlite[0], serde_json::json!([4, ["e", "b", "d", "a"]]));
        for other in &observed[1..] {
            assert_eq!(other, sqlite);
        }
    }

    /// A corpus searched through `db`'s full-text index: every ranked and
    /// paged answer, scores to ten significant digits so float noise cannot
    /// fail the comparison but a real difference in ranking does.
    /// Every search the node makes, on a fixed corpus, as comparable values.
    /// With `tombstones`, one row is superseded and one deleted, and hits
    /// are compared as sets of ids: FTS5's BM25 counts those rows, the
    /// engine's does not, so only the matches must agree.
    fn exercise_search(db: &Database, tombstones: bool) -> Vec<Value> {
        const T2: &str = "2026-09-27T00:00:00+00:00";
        let store = db.store();
        let m = Memories::new(&store);
        let text = |s: &str| Some(s.to_string());
        let corpus = [
            (
                "a",
                "the quokka eats leaves at night",
                "fact",
                vec!["animal"],
                NOW,
            ),
            (
                "b",
                "a quokka and a wombat share a burrow",
                "fact",
                vec!["animal", "burrow"],
                T2,
            ),
            (
                "c",
                "wombat burrows are deep; quokka burrows are not",
                "note",
                vec![],
                T2,
            ),
            (
                "d",
                "rust ownership rules and the borrow checker",
                "note",
                vec!["rust"],
                NOW,
            ),
            ("e", "quokka quokka quokka everywhere", "fact", vec![], NOW),
            ("f", "a sensitive quokka secret", "fact", vec![], NOW),
            ("g", "a dormant quokka nobody reads", "fact", vec![], NOW),
            ("h", "a superseded quokka note", "fact", vec![], NOW),
            ("i", "a deleted quokka note", "fact", vec![], NOW),
            (
                "j",
                "the borrow checker loves wombats",
                "note",
                vec!["quokka"],
                T2,
            ),
        ];
        for (id, content, category, tags, at) in corpus {
            m.insert(&NewMemory {
                category: category.into(),
                tags: tags.into_iter().map(String::from).collect(),
                sensitive: id == "f",
                base_weight: if id == "g" { 0.001 } else { 1.0 },
                subject: if id == "d" { text("Rust") } else { None },
                ..NewMemory::new(id, content, at)
            })
            .unwrap();
        }
        if tombstones {
            m.set_superseded_by("h", "a", None).unwrap();
            m.delete_live("i", Some(T2)).unwrap();
        }
        crate::db::entities::Entities::new(&store)
            .insert(
                &crate::entity::Entity {
                    id: "ent_w".into(),
                    name: "Wombat".into(),
                    kind: None,
                    aliases: Vec::new(),
                    created_at: NOW.into(),
                    updated_at: NOW.into(),
                },
                None,
            )
            .unwrap();
        crate::db::entities::Entities::new(&store)
            .link("c", "ent_w", NOW, Origin::Local)
            .unwrap();

        let phrases = crate::fts::query_phrases;
        let mut seen = Vec::new();
        let queries = [
            "quokka",
            "quokka burrow",
            "\"borrow checker\"",
            "what's a wombat?",
            "animal",
            "note",
            "nothing matches this",
        ];
        let filters = [
            KeywordFilter::default(),
            KeywordFilter {
                include_sensitive: true,
                ..KeywordFilter::default()
            },
            KeywordFilter {
                min_effective_vitality: Some(crate::vitality::VITALITY_FLOOR),
                category: text("fact"),
                include_sensitive: true,
            },
        ];
        for query in queries {
            for filter in &filters {
                for limit in [2, 20] {
                    let hits = m.keyword_hits(&phrases(query), filter, limit).unwrap();
                    if !tombstones {
                        let hits: Vec<Value> = hits
                            .into_iter()
                            .map(|(memory, score)| {
                                serde_json::json!([memory.id, format!("{score:.9e}")])
                            })
                            .collect();
                        seen.push(serde_json::json!([query, limit, hits]));
                    } else if limit == 20 {
                        let mut ids: Vec<String> =
                            hits.into_iter().map(|(memory, _)| memory.id).collect();
                        ids.sort();
                        seen.push(serde_json::json!([query, ids]));
                    }
                }
            }
        }
        let pages = [
            ("quokka", PageFilter::default(), 3, 0),
            ("quokka", PageFilter::default(), 3, 3),
            (
                "quokka",
                PageFilter {
                    category: text("fact"),
                    tags: vec!["animal".into()],
                    entity: None,
                },
                10,
                0,
            ),
            ("", PageFilter::default(), 4, 1),
            (
                "",
                PageFilter {
                    entity: Some(EntityScope {
                        id: "ent_w".into(),
                        canonical: "rust".into(),
                    }),
                    ..PageFilter::default()
                },
                10,
                0,
            ),
            (
                "burrows",
                PageFilter {
                    entity: Some(EntityScope {
                        id: "ent_w".into(),
                        canonical: "wombat".into(),
                    }),
                    ..PageFilter::default()
                },
                10,
                0,
            ),
        ];
        for (query, filter, limit, offset) in pages {
            let (total, page) = m
                .keyword_page(&phrases(query), &filter, limit, offset)
                .unwrap();
            let ids: Vec<String> = page.into_iter().map(|m| m.id).collect();
            if tombstones {
                seen.push(serde_json::json!([query, total]));
            } else {
                seen.push(serde_json::json!([query, total, ids]));
            }
        }
        let mut sensitive: Vec<String> = m.sensitive_ids().unwrap().into_iter().collect();
        sensitive.sort();
        seen.push(serde_json::json!(sensitive));
        seen
    }

    #[test]
    fn the_engine_core_searches_as_fts5_does() {
        let mut observed = Vec::new();
        on_each_backend(|db| observed.push(exercise_search(db, false)));
        #[cfg(feature = "engine-store")]
        assert_eq!(observed.len(), 2, "both backends ran");
        let sqlite = &observed[0];
        assert_eq!(
            sqlite[0][2].as_array().map(Vec::len),
            Some(2),
            "the corpus exercises a real ranking: {:?}",
            sqlite[0]
        );
        for other in &observed[1..] {
            for (theirs, ours) in other.iter().zip(sqlite) {
                assert_eq!(theirs, ours);
            }
            assert_eq!(other.len(), sqlite.len());
        }
    }

    /// FTS5 keeps superseded and deleted rows in its index and counts them
    /// in BM25; the engine indexes live rows only. Search keeps live rows
    /// alone either way, so both find the same memories.
    #[test]
    fn with_tombstones_the_engine_core_finds_what_fts5_finds() {
        let mut observed = Vec::new();
        on_each_backend(|db| observed.push(exercise_search(db, true)));
        #[cfg(feature = "engine-store")]
        assert_eq!(observed.len(), 2, "both backends ran");
        let sqlite = &observed[0];
        assert!(
            sqlite
                .iter()
                .any(|v| v[1].as_array().is_some_and(|ids| ids.len() > 2)),
            "the corpus gives some query several hits: {sqlite:?}"
        );
        for other in &observed[1..] {
            assert_eq!(other, sqlite);
        }
    }

    #[test]
    fn the_engine_core_matches_sqlite_write_for_write() {
        let mut observed = Vec::new();
        on_each_backend(|db| observed.push(exercise(db)));
        let sqlite = &observed[0];
        assert_eq!(sqlite.counts, [1, 0, 2, 2, 3]);
        assert!(!sqlite.outbox.is_empty());
        for other in &observed[1..] {
            assert_eq!(other, sqlite);
        }
    }
}
