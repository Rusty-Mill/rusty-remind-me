//! Memories on the engine (ADR-0023, core PR 1): the rows, the
//! full-text and tag indexes derived from them, and every write
//! [`crate::db::memories`] makes, with its outbox entry committed in the
//! same journal batch.
//!
//! A row keeps each column as SQLite stores it: `tags` and `metadata` stay
//! JSON text, so a malformed value reads back the same way on both
//! backends, and a nullable column stays an `Option`. The indexes are never
//! stored; they are rebuilt from the rows at open and kept in step by every
//! batch that touches a memory, as `db::derived` keeps FTS5 and
//! `memory_tags` in step on SQLite.

use super::core::{Change, CoreTables};
use super::EngineTables;
use super::{core_mut, core_ref, engine_error, engine_id, ensure_same_id, micros, outbox};
use crate::db::derived::Origin;
use crate::db::feedback::Importance;
use crate::db::history::Tracked;
use crate::db::memories::{
    tags_json, AccessInputs, CreatedMemory, KeywordFilter, ListFilter, MemoryEdit, NewMemory,
    PageFilter, SyncView, Triple,
};
use crate::db::Result;
use crate::models::{Memory, UnannotatedMemory, UnclassifiedMemory};
use rusty_multimodal_db_engine::fulltext::{FullTextIndex, Query};
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Index marker: a memory by the import document it is a chunk of (empty
/// for a memory that is not one).
pub struct ByDoc;
/// Slot marker: when the memory was created, in µs since the epoch.
pub struct CreatedAt;

pub(crate) type MemoryTable = GenericMmapStore<MemoryRecord, ByDoc, CreatedAt>;

/// The full-text index over (content, category, tags), as `memories_fts`
/// indexes them; keyed by memory id.
pub(crate) type MemorySearch = FullTextIndex<String, 3>;

/// One `memories` row, every column as SQLite stores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRow {
    pub(crate) id: String,
    pub(crate) content: String,
    pub(crate) category: String,
    /// The `tags` column's JSON text.
    pub(crate) tags: String,
    pub(crate) source: String,
    /// The `metadata` column's JSON text.
    pub(crate) metadata: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) capture_id: Option<String>,
    pub(crate) subject: Option<String>,
    pub(crate) predicate: Option<String>,
    pub(crate) object: Option<String>,
    pub(crate) superseded_by: Option<String>,
    pub(crate) decay_rate: f64,
    pub(crate) vitality: f64,
    pub(crate) base_weight: f64,
    pub(crate) access_count: i64,
    pub(crate) accessed_at: Option<String>,
    pub(crate) doc_id: Option<String>,
    pub(crate) chunk_index: Option<i64>,
    pub(crate) remind_at: Option<String>,
    pub(crate) sensitive: bool,
    pub(crate) memory_type: String,
    pub(crate) status: String,
    pub(crate) node_id: Option<String>,
    pub(crate) client: String,
    pub(crate) source_capture_id: Option<String>,
    pub(crate) deleted_at: Option<String>,
}

impl MemoryRow {
    /// `row` as an `INSERT` of it stores it.
    pub(crate) fn from_new(row: &NewMemory) -> Self {
        Self {
            id: row.id.clone(),
            content: row.content.clone(),
            category: row.category.clone(),
            tags: tags_json(&row.tags),
            source: row.source.clone(),
            metadata: row.metadata.to_string(),
            created_at: row.created_at.clone(),
            updated_at: row.updated_at.clone(),
            capture_id: row.capture_id.clone(),
            subject: row.subject.clone(),
            predicate: row.predicate.clone(),
            object: row.object.clone(),
            superseded_by: row.superseded_by.clone(),
            decay_rate: row.decay_rate,
            vitality: row.vitality,
            base_weight: row.base_weight,
            access_count: row.access_count,
            accessed_at: row.accessed_at.clone(),
            doc_id: row.doc_id.clone(),
            chunk_index: row.chunk_index,
            remind_at: row.remind_at.clone(),
            sensitive: row.sensitive,
            memory_type: row.memory_type.clone(),
            status: row.status.clone(),
            node_id: row.node_id.clone(),
            client: row.client.clone(),
            source_capture_id: row.source_capture_id.clone(),
            deleted_at: row.deleted_at.clone(),
        }
    }

    /// The row as `parse_memory_row` reads it: unparseable tags as none,
    /// unparseable metadata as `null`, and a missing `accessed_at` as
    /// `created_at`.
    pub(crate) fn to_memory(&self) -> Memory {
        Memory {
            id: self.id.clone(),
            content: self.content.clone(),
            category: self.category.clone(),
            tags: serde_json::from_str(&self.tags).unwrap_or_default(),
            source: self.source.clone(),
            metadata: serde_json::from_str(&self.metadata).unwrap_or(Value::Null),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            capture_id: self.capture_id.clone(),
            subject: self.subject.clone(),
            predicate: self.predicate.clone(),
            object: self.object.clone(),
            superseded_by: self.superseded_by.clone(),
            decay_rate: self.decay_rate,
            vitality: self.vitality,
            base_weight: self.base_weight,
            access_count: self.access_count,
            accessed_at: self
                .accessed_at
                .clone()
                .unwrap_or_else(|| self.created_at.clone()),
            doc_id: self.doc_id.clone(),
            chunk_index: self.chunk_index,
            remind_at: self.remind_at.clone(),
            sensitive: self.sensitive,
            memory_type: Some(self.memory_type.clone()),
            status: Some(self.status.clone()),
            node_id: self.node_id.clone(),
            client: Some(self.client.clone()),
            source_capture_id: self.source_capture_id.clone(),
            deleted_at: self.deleted_at.clone(),
        }
    }

    /// The outbox payload `db::derived` builds with `json_object`: the same
    /// 28 keys, tags and metadata as their JSON text, `sensitive` as 0 or 1.
    pub(crate) fn payload(&self) -> Value {
        let mut payload = self.backfill_payload();
        if let Value::Object(fields) = &mut payload {
            fields.insert("remind_at".into(), self.remind_at.clone().into());
            fields.insert("sensitive".into(), i64::from(self.sensitive).into());
        }
        payload
    }

    /// The payload `db::outbox`'s backfill builds, which has never carried
    /// `remind_at` or `sensitive`.
    pub(crate) fn backfill_payload(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "content": self.content,
            "category": self.category,
            "tags": self.tags,
            "source": self.source,
            "metadata": self.metadata,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "capture_id": self.capture_id,
            "node_id": self.node_id,
            "client": self.client,
            "accessed_at": self.accessed_at,
            "access_count": self.access_count,
            "decay_rate": self.decay_rate,
            "vitality": self.vitality,
            "base_weight": self.base_weight,
            "status": self.status,
            "memory_type": self.memory_type,
            "source_capture_id": self.source_capture_id,
            "subject": self.subject,
            "predicate": self.predicate,
            "object": self.object,
            "superseded_by": self.superseded_by,
            "doc_id": self.doc_id,
            "chunk_index": self.chunk_index,
            "deleted_at": self.deleted_at,
        })
    }

    /// The tags `memory_tags` holds for this row: every text value
    /// `json_each(tags)` yields, which for an object is its values and for
    /// a lone string is itself. None when the text is not JSON.
    fn tag_values(&self) -> Vec<String> {
        let text = |v: &Value| v.as_str().map(str::to_string);
        match serde_json::from_str::<Value>(&self.tags) {
            Ok(Value::Array(items)) => items.iter().filter_map(text).collect(),
            Ok(Value::Object(fields)) => fields.values().filter_map(text).collect(),
            Ok(Value::String(tag)) => vec![tag],
            _ => Vec::new(),
        }
    }

    fn is_live(&self) -> bool {
        self.deleted_at.is_none() && self.superseded_by.is_none()
    }

    /// The parsed metadata, or `None` when it is not JSON: where SQLite's
    /// `json_extract` would fail, the engine reads no value.
    fn metadata_value(&self) -> Option<Value> {
        serde_json::from_str(&self.metadata).ok()
    }
}

/// A memory as the engine stores it: its row, plus the keys its index and
/// slot are read from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    engine_id: Uuid,
    doc_key: String,
    created_us: i64,
    pub(crate) row: MemoryRow,
}

impl MemoryRecord {
    pub(crate) fn new(row: MemoryRow) -> Self {
        Self {
            engine_id: engine_id(&row.id),
            doc_key: row.doc_id.clone().unwrap_or_default(),
            created_us: micros(&row.created_at),
            row,
        }
    }
}

impl Record for MemoryRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for MemoryRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::MemoryRecord@1";
}

impl IndexedField<ByDoc> for MemoryRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.doc_key
    }
}

impl ScannableField<CreatedAt> for MemoryRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.created_us
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.created_us = value;
    }
}

// --- derived indexes ------------------------------------------------------

/// `memory_tags`: which memories carry each tag.
#[derive(Debug, Default)]
pub(crate) struct TagIndex(HashMap<String, HashSet<String>>);

impl TagIndex {
    /// Whether memory `id` carries `tag`.
    pub(crate) fn has(&self, tag: &str, id: &str) -> bool {
        self.0.get(tag).is_some_and(|ids| ids.contains(id))
    }

    /// Every tag, with the ids of the memories carrying it.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&String, &HashSet<String>)> {
        self.0.iter()
    }

    fn add(&mut self, row: &MemoryRow) {
        for tag in row.tag_values() {
            self.0.entry(tag).or_default().insert(row.id.clone());
        }
    }

    fn remove(&mut self, row: &MemoryRow) {
        for tag in row.tag_values() {
            if let Some(ids) = self.0.get_mut(&tag) {
                ids.remove(&row.id);
                if ids.is_empty() {
                    self.0.remove(&tag);
                }
            }
        }
    }
}

/// Put `row` into the derived indexes.
pub(crate) fn index(search: &mut MemorySearch, tags: &mut TagIndex, row: &MemoryRow) {
    search.upsert(row.id.clone(), [&row.content, &row.category, &row.tags]);
    tags.add(row);
}

/// Take `row` out of the derived indexes.
pub(crate) fn unindex(search: &mut MemorySearch, tags: &mut TagIndex, row: &MemoryRow) {
    search.remove(&row.id);
    tags.remove(row);
}

/// The derived indexes over every row of `table`, built at open.
pub(crate) fn index_all(table: &MemoryTable) -> (MemorySearch, TagIndex) {
    let mut search = MemorySearch::new();
    let mut tags = TagIndex::default();
    for id in table.all_ids() {
        if let Some(record) = table.get(id) {
            index(&mut search, &mut tags, &record.row);
        }
    }
    (search, tags)
}

// --- writes ---------------------------------------------------------------

/// What a write does to one memory, given the row as it is stored now.
pub(crate) enum Edit {
    /// Nothing: the row stays as it is, or stays missing.
    Keep,
    /// Store this row, inserting it or replacing the one there.
    Put(Box<MemoryRow>),
    /// Remove the row.
    Delete,
}

/// Apply `edit` to memory `id` in one journal batch, with the outbox entry
/// `db::derived::write_memory` would queue: a local write that creates the
/// row queues an `insert`, one that moves `updated_at` an `update`, and
/// only while the `sync_enabled` flag is `'1'`.
pub(crate) fn write(
    tables: &mut EngineTables,
    id: &str,
    origin: Origin,
    edit: impl FnOnce(Option<&MemoryRow>) -> Result<Edit>,
) -> Result<()> {
    let before = stored(core_ref(tables)?, id)?;
    let after = match edit(before.as_ref())? {
        Edit::Keep => return Ok(()),
        Edit::Delete if before.is_none() => return Ok(()),
        Edit::Delete => None,
        Edit::Put(row) => Some(*row),
    };
    let mut changes = Vec::new();
    if let Some(row) = &after {
        if row.id != id {
            return Err(engine_error(format!(
                "a write to memory {id:?} tried to store {:?}",
                row.id
            )));
        }
        if let Some(operation) = queued_operation(before.as_ref(), row, origin) {
            if outbox::sync_enabled(core_ref(tables)?) {
                let payload = row.payload().to_string();
                changes.push(outbox::entry(tables, id, operation, payload)?);
            }
        }
    }
    changes.insert(
        0,
        Change::Memory(
            engine_id(id),
            after.map(|row| Box::new(MemoryRecord::new(row))),
        ),
    );
    tables.commit(changes)
}

/// The outbox operation a write from `origin` that turns `before` into
/// `after` queues, if any.
fn queued_operation(
    before: Option<&MemoryRow>,
    after: &MemoryRow,
    origin: Origin,
) -> Option<&'static str> {
    if origin == Origin::Sync {
        return None;
    }
    match before {
        None => Some("insert"),
        Some(old) if old.updated_at != after.updated_at => Some("update"),
        Some(_) => None,
    }
}

/// Apply `change` to memory `id` if it exists, as an `UPDATE … WHERE id`.
fn update(
    tables: &mut EngineTables,
    id: &str,
    origin: Origin,
    change: impl FnOnce(&mut MemoryRow) -> Result<()>,
) -> Result<()> {
    write(tables, id, origin, |before| {
        let Some(row) = before else {
            return Ok(Edit::Keep);
        };
        let mut row = row.clone();
        change(&mut row)?;
        Ok(Edit::Put(Box::new(row)))
    })
}

pub(crate) fn insert(tables: &mut EngineTables, new: &NewMemory) -> Result<()> {
    write(tables, &new.id, Origin::Local, |before| match before {
        Some(_) => Err(engine_error(format!("memory {:?} already exists", new.id))),
        None => Ok(Edit::Put(Box::new(MemoryRow::from_new(new)))),
    })
}

pub(crate) fn insert_or_ignore(tables: &mut EngineTables, new: &NewMemory) -> Result<bool> {
    let mut inserted = false;
    write(tables, &new.id, Origin::Local, |before| {
        if before.is_some() {
            return Ok(Edit::Keep);
        }
        inserted = true;
        Ok(Edit::Put(Box::new(MemoryRow::from_new(new))))
    })?;
    Ok(inserted)
}

/// Insert a record a peer sent, or overwrite the local row with it keeping
/// the local `created_at`, `doc_id` and `chunk_index`.
pub(crate) fn upsert_synced(tables: &mut EngineTables, new: &NewMemory) -> Result<()> {
    write(tables, &new.id, Origin::Sync, |before| {
        let mut row = MemoryRow::from_new(new);
        if let Some(local) = before {
            row.created_at = local.created_at.clone();
            row.doc_id = local.doc_id.clone();
            row.chunk_index = local.chunk_index;
        }
        Ok(Edit::Put(Box::new(row)))
    })
}

pub(crate) fn set_tags_and_metadata(
    tables: &mut EngineTables,
    id: &str,
    tags: &[String],
    metadata: &Value,
) -> Result<()> {
    update(tables, id, Origin::Sync, |row| {
        row.tags = tags_json(tags);
        row.metadata = metadata.to_string();
        Ok(())
    })
}

pub(crate) fn set_superseded_by(
    tables: &mut EngineTables,
    id: &str,
    superseded_by: &str,
    updated_at: Option<&str>,
) -> Result<()> {
    update(tables, id, Origin::Local, |row| {
        row.superseded_by = Some(superseded_by.to_string());
        if let Some(stamp) = updated_at {
            row.updated_at = stamp.to_string();
        }
        Ok(())
    })
}

/// Supersede every live, unsuperseded chunk whose `metadata.import_id` is
/// the text `old_import_id`. How many were.
pub(crate) fn supersede_import(
    tables: &mut EngineTables,
    old_import_id: &str,
    new_import_id: &str,
    updated_at: &str,
) -> Result<usize> {
    let ids = matching(core_ref(tables)?, |row| {
        row.is_live()
            && row
                .metadata_value()
                .is_some_and(|m| m.get("import_id").and_then(Value::as_str) == Some(old_import_id))
    });
    for id in &ids {
        set_superseded_by(tables, id, new_import_id, Some(updated_at))?;
    }
    Ok(ids.len())
}

pub(crate) fn delete_capture_category(
    tables: &mut EngineTables,
    capture_id: &str,
    category: &str,
) -> Result<usize> {
    let ids = matching(core_ref(tables)?, |row| {
        row.capture_id.as_deref() == Some(capture_id) && row.category == category
    });
    for id in &ids {
        write(tables, id, Origin::Local, |_| Ok(Edit::Delete))?;
    }
    Ok(ids.len())
}

pub(crate) fn set_merged(
    tables: &mut EngineTables,
    id: &str,
    content: &str,
    access_count: i64,
    tags: &[String],
    updated_at: &str,
) -> Result<()> {
    update(tables, id, Origin::Local, |row| {
        row.content = content.to_string();
        row.access_count = access_count;
        row.tags = tags_json(tags);
        row.updated_at = updated_at.to_string();
        Ok(())
    })
}

pub(crate) fn set_vitality(
    tables: &mut EngineTables,
    id: &str,
    vitality: f64,
    status: &str,
) -> Result<()> {
    update(tables, id, Origin::Local, |row| {
        row.vitality = vitality;
        row.status = status.to_string();
        Ok(())
    })
}

pub(crate) fn record_access(
    tables: &mut EngineTables,
    id: &str,
    accessed_at: &str,
    access_count: i64,
    vitality: f64,
    status: &str,
) -> Result<()> {
    update(tables, id, Origin::Local, |row| {
        row.accessed_at = Some(accessed_at.to_string());
        row.access_count = access_count;
        row.vitality = vitality;
        row.status = status.to_string();
        Ok(())
    })
}

/// Set `metadata.ingest` on every chunk of `doc_id`, as `json_set` does: a
/// metadata object gains or replaces the key, any other JSON value is left
/// as it is, and text that is not JSON is an error.
pub(crate) fn set_ingest_marker(
    tables: &mut EngineTables,
    doc_id: &str,
    marker: &str,
) -> Result<usize> {
    let ids = FilterEq::<MemoryRecord, ByDoc>::filter_eq(
        &core_ref(tables)?.memories,
        &doc_id.to_string(),
    );
    let ids: Vec<String> = ids
        .into_iter()
        .filter_map(|eid| core_ref(tables).ok()?.memories.get(eid))
        .filter(|record| record.row.doc_id.as_deref() == Some(doc_id))
        .map(|record| record.row.id)
        .collect();
    for id in &ids {
        update(tables, id, Origin::Local, |row| {
            let mut metadata: Value = serde_json::from_str(&row.metadata)
                .map_err(|e| engine_error(format!("memory {:?} metadata: {e}", row.id)))?;
            if let Value::Object(fields) = &mut metadata {
                fields.insert("ingest".to_string(), Value::String(marker.to_string()));
                row.metadata = metadata.to_string();
            }
            Ok(())
        })?;
    }
    Ok(ids.len())
}

/// Apply `edit` to memory `id`, as `Memories::apply_edit`'s `UPDATE`.
pub(crate) fn apply_edit(tables: &mut EngineTables, id: &str, edit: &MemoryEdit) -> Result<()> {
    update(tables, id, Origin::Local, |row| {
        let set = |field: &mut String, value: &Option<String>| {
            if let Some(v) = value {
                field.clone_from(v);
            }
        };
        set(&mut row.content, &edit.content);
        set(&mut row.category, &edit.category);
        set(&mut row.memory_type, &edit.memory_type);
        if let Some(tags) = &edit.tags {
            row.tags = tags_json(tags);
        }
        if let Some(metadata) = &edit.metadata {
            row.metadata = metadata.to_string();
        }
        for (field, value) in [
            (&mut row.subject, &edit.subject),
            (&mut row.predicate, &edit.predicate),
            (&mut row.object, &edit.object),
        ] {
            if value.is_some() {
                field.clone_from(value);
            }
        }
        if let Some(sensitive) = edit.sensitive {
            row.sensitive = sensitive;
        }
        if let Some(rate) = edit.decay_rate {
            row.decay_rate = rate;
        }
        if edit.clear_superseded {
            row.superseded_by = None;
        }
        row.updated_at.clone_from(&edit.updated_at);
        Ok(())
    })
}

/// Write `values` into memory `id`'s tracked columns as stored text,
/// stamping `updated_at`: a revert. A `None` `sensitive` writes not
/// sensitive.
pub(crate) fn restore_tracked(
    tables: &mut EngineTables,
    id: &str,
    values: &Tracked,
    updated_at: &str,
) -> Result<()> {
    update(tables, id, Origin::Local, |row| {
        row.content.clone_from(&values.content);
        row.category.clone_from(&values.category);
        row.tags.clone_from(&values.tags);
        row.metadata.clone_from(&values.metadata);
        row.sensitive = values.sensitive.unwrap_or(false);
        row.updated_at = updated_at.to_string();
        Ok(())
    })
}

/// Set or clear memory `id`'s reminder, stamping `updated_at`.
pub(crate) fn set_remind_at(
    tables: &mut EngineTables,
    id: &str,
    remind_at: Option<&str>,
    updated_at: &str,
) -> Result<()> {
    update(tables, id, Origin::Local, |row| {
        row.remind_at = remind_at.map(str::to_string);
        row.updated_at = updated_at.to_string();
        Ok(())
    })
}

/// Rewrite memory `id`'s importance after a global judgement, without
/// stamping `updated_at`: a local score, not an edit.
pub(crate) fn set_importance(
    tables: &mut EngineTables,
    id: &str,
    base_weight: f64,
    vitality: f64,
    status: &str,
) -> Result<()> {
    update(tables, id, Origin::Local, |row| {
        row.base_weight = base_weight;
        row.vitality = vitality;
        row.status = status.to_string();
        Ok(())
    })
}

/// Delete live memory `id`: tombstone it at `tombstone_at`, or remove it.
/// Whether it was live.
pub(crate) fn delete_live(
    tables: &mut EngineTables,
    id: &str,
    tombstone_at: Option<&str>,
) -> Result<bool> {
    let mut deleted = false;
    write(tables, id, Origin::Local, |before| {
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
        Ok(Edit::Put(Box::new(row)))
    })?;
    Ok(deleted)
}

// --- reads ----------------------------------------------------------------

/// Memory `id`'s row as stored, refusing a different id at its engine id.
fn stored(core: &CoreTables, id: &str) -> Result<Option<MemoryRow>> {
    let Some(record) = core.memories.get(engine_id(id)) else {
        return Ok(None);
    };
    ensure_same_id(&record.row.id, id)?;
    Ok(Some(record.row))
}

/// Memory `id`'s row, if there is one.
pub(crate) fn row(core: &CoreTables, id: &str) -> Option<MemoryRow> {
    core.memories
        .get(engine_id(id))
        .map(|record| record.row)
        .filter(|row| row.id == id)
}

/// Every row, in no particular order.
pub(crate) fn rows(core: &CoreTables) -> impl Iterator<Item = MemoryRow> + '_ {
    core.memories
        .all_ids()
        .into_iter()
        .filter_map(|id| core.memories.get(id))
        .map(|record| record.row)
}

/// The ids of the rows `keep` selects.
fn matching(core: &CoreTables, keep: impl Fn(&MemoryRow) -> bool) -> Vec<String> {
    let mut ids: Vec<String> = rows(core).filter(|r| keep(r)).map(|r| r.id).collect();
    ids.sort();
    ids
}

pub(crate) fn sync_view(tables: &EngineTables, id: &str) -> Result<Option<SyncView>> {
    Ok(row(core_ref(tables)?, id).map(|row| SyncView {
        tags: serde_json::from_str(&row.tags).unwrap_or_default(),
        metadata: serde_json::from_str(&row.metadata)
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
        updated_at: row.updated_at,
    }))
}

pub(crate) fn access_inputs(tables: &EngineTables, ids: &[String]) -> Result<Vec<AccessInputs>> {
    let core = core_ref(tables)?;
    Ok(distinct(ids)
        .filter_map(|id| row(core, id))
        .map(|row| AccessInputs {
            id: row.id,
            access_count: row.access_count,
            decay_rate: row.decay_rate,
            base_weight: row.base_weight,
        })
        .collect())
}

pub(crate) fn live_triples_except(tables: &EngineTables, except_id: &str) -> Result<Vec<Triple>> {
    let mut triples: Vec<Triple> = rows(core_ref(tables)?)
        .filter(|row| row.id != except_id && row.is_live())
        .filter_map(|row| {
            Some(Triple {
                subject: row.subject?,
                predicate: row.predicate?,
                object: row.object?,
                id: row.id,
            })
        })
        .collect();
    triples.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(triples)
}

/// Live memories created after `cutoff` as SQLite compares the text,
/// oldest first (ties by id), at most `limit`.
pub(crate) fn live_created_after(
    tables: &EngineTables,
    cutoff: &str,
    limit: usize,
) -> Result<Vec<CreatedMemory>> {
    let mut rows: Vec<MemoryRow> = rows(core_ref(tables)?)
        .filter(|row| row.is_live() && row.created_at.as_str() > cutoff)
        .collect();
    rows.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    Ok(rows
        .into_iter()
        .take(limit)
        .map(|row| CreatedMemory {
            id: row.id,
            category: row.category,
            content: row.content,
            created_at: row.created_at,
        })
        .collect())
}

pub(crate) fn count_live_created_after(tables: &EngineTables, cutoff: &str) -> Result<usize> {
    Ok(rows(core_ref(tables)?)
        .filter(|row| row.is_live() && row.created_at.as_str() > cutoff)
        .count())
}

pub(crate) fn get_many(tables: &EngineTables, ids: &[String]) -> Result<Vec<Memory>> {
    let core = core_ref(tables)?;
    Ok(distinct(ids)
        .filter_map(|id| row(core, id))
        .map(|row| row.to_memory())
        .collect())
}

pub(crate) fn exists(tables: &EngineTables, id: &str) -> Result<bool> {
    Ok(row(core_ref(tables)?, id).is_some())
}

pub(crate) fn sensitivity(tables: &EngineTables, id: &str) -> Result<Option<bool>> {
    Ok(row(core_ref(tables)?, id).map(|row| row.sensitive))
}

/// What `Memories::exportable` selects, oldest first, ties by id.
pub(crate) fn exportable(
    tables: &EngineTables,
    include_deleted: bool,
    category: Option<&str>,
    tags: &[String],
) -> Result<Vec<Memory>> {
    let core = core_ref(tables)?;
    let mut rows: Vec<MemoryRow> = rows(core)
        .filter(|row| include_deleted || row.is_live())
        .filter(|row| category.is_none_or(|c| row.category == c))
        .filter(|row| tags.iter().all(|tag| core.tags.has(tag, &row.id)))
        .collect();
    rows.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    Ok(rows.iter().map(MemoryRow::to_memory).collect())
}

pub(crate) fn all_live(tables: &EngineTables) -> Result<Vec<Memory>> {
    Ok(rows(core_ref(tables)?)
        .filter(|row| row.deleted_at.is_none())
        .map(|row| row.to_memory())
        .collect())
}

/// Live, unsuperseded, non-sensitive memories whose metadata has a non-null
/// `code_refs`: id, content and metadata text.
pub(crate) fn with_code_refs(tables: &EngineTables) -> Result<Vec<(String, String, String)>> {
    let mut found: Vec<(String, String, String)> = rows(core_ref(tables)?)
        .filter(|row| row.is_live() && !row.sensitive)
        .filter(|row| {
            row.metadata_value()
                .is_some_and(|m| m.get("code_refs").is_some_and(|v| !v.is_null()))
        })
        .map(|row| (row.id, row.content, row.metadata))
        .collect();
    found.sort();
    Ok(found)
}

/// `ids` without repeats, first occurrence first: an `IN (…)` list matches
/// each row once however often it is named.
fn distinct(ids: &[String]) -> impl Iterator<Item = &str> {
    let mut seen = HashSet::new();
    ids.iter()
        .map(String::as_str)
        .filter(move |id| seen.insert(*id))
}

/// Memory `id`'s tracked columns as stored, deleted or not.
pub(crate) fn tracked(tables: &EngineTables, id: &str) -> Result<Option<Tracked>> {
    Ok(row(core_ref(tables)?, id).map(|row| Tracked {
        content: row.content,
        category: row.category,
        tags: row.tags,
        metadata: row.metadata,
        sensitive: Some(row.sensitive),
    }))
}

/// Memory `id`'s importance columns, unless it is missing or deleted.
pub(crate) fn importance(tables: &EngineTables, id: &str) -> Result<Option<Importance>> {
    Ok(row(core_ref(tables)?, id)
        .filter(|row| row.deleted_at.is_none())
        .map(|row| Importance {
            access_count: row.access_count,
            base_weight: row.base_weight,
            vitality: row.vitality,
        }))
}

/// Memory `id`, unless it is missing or deleted.
pub(crate) fn get_live(tables: &EngineTables, id: &str) -> Result<Option<Memory>> {
    Ok(row(core_ref(tables)?, id)
        .filter(|row| row.deleted_at.is_none())
        .map(|row| row.to_memory()))
}

/// What `Memories::list_page` selects: the total, and the page, newest
/// first by the `created_at` text, ties by id descending.
pub(crate) fn list_page(
    tables: &EngineTables,
    filter: &ListFilter,
    limit: usize,
    offset: usize,
) -> Result<(usize, Vec<Memory>)> {
    let core = core_ref(tables)?;
    let mut rows: Vec<MemoryRow> = rows(core)
        .filter(|row| row.deleted_at.is_none())
        .filter(|row| filter.include_sensitive || !row.sensitive)
        .filter(|row| filter.category.as_ref().is_none_or(|c| &row.category == c))
        .filter(|row| filter.source.as_ref().is_none_or(|s| &row.source == s))
        .filter(|row| filter.tags.iter().all(|tag| core.tags.has(tag, &row.id)))
        .collect();
    rows.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));
    let total = rows.len();
    let page = rows
        .iter()
        .skip(offset)
        .take(limit)
        .map(MemoryRow::to_memory)
        .collect();
    Ok((total, page))
}

/// What `Memories::of_type_page` selects: live memories of `memory_type`,
/// oldest first, ties by id, with the first 500 characters of the content
/// as SQLite's `substr` takes them.
pub(crate) fn of_type_page(
    tables: &EngineTables,
    memory_type: &str,
    limit: usize,
) -> Result<(usize, Vec<UnclassifiedMemory>)> {
    let mut rows: Vec<MemoryRow> = rows(core_ref(tables)?)
        .filter(|row| row.deleted_at.is_none() && row.memory_type == memory_type)
        .collect();
    rows.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
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

/// Every live, unsuperseded row matching any of `phrases`, with its BM25
/// score, best first, ties by id: `memories_fts MATCH … ORDER BY bm25()`.
fn ranked(core: &CoreTables, phrases: &[String]) -> Vec<(MemoryRow, f64)> {
    let query = Query::any_of(phrases.iter().map(String::as_str));
    let mut hits: Vec<(MemoryRow, f64)> = core
        .search
        .search(&query)
        .into_iter()
        .filter_map(|hit| Some((row(core, &hit.key)?, hit.score)))
        .filter(|(row, _)| row.is_live())
        .collect();
    hits.sort_by(|(a, x), (b, y)| x.total_cmp(y).then_with(|| a.id.cmp(&b.id)));
    hits
}

/// What `Memories::keyword_hits` selects. Effective vitality is computed as
/// of one instant for the whole search, where SQLite reads the clock per
/// row.
pub(crate) fn keyword_hits(
    tables: &EngineTables,
    phrases: &[String],
    filter: &KeywordFilter,
    limit: usize,
) -> Result<Vec<(Memory, f64)>> {
    let now = chrono::Utc::now();
    Ok(ranked(core_ref(tables)?, phrases)
        .into_iter()
        .filter(|(row, _)| filter.include_sensitive || !row.sensitive)
        .filter(|(row, _)| filter.category.as_ref().is_none_or(|c| &row.category == c))
        .map(|(row, score)| (row.to_memory(), score))
        .filter(|(memory, _)| {
            filter
                .min_effective_vitality
                .is_none_or(|floor| crate::vitality::effective_vitality(memory, now) >= floor)
        })
        .take(limit)
        .collect())
}

pub(crate) fn sensitive_ids(tables: &EngineTables) -> Result<HashSet<String>> {
    Ok(rows(core_ref(tables)?)
        .filter(|row| row.sensitive)
        .map(|row| row.id)
        .collect())
}

/// What `Memories::keyword_page` selects. `linked` holds the ids of the
/// memories linked to the filter's entity, read from SQLite.
pub(crate) fn keyword_page(
    tables: &EngineTables,
    phrases: &[String],
    filter: &PageFilter,
    linked: &HashSet<String>,
    limit: usize,
    offset: usize,
) -> Result<(usize, Vec<Memory>)> {
    let core = core_ref(tables)?;
    let candidates: Vec<MemoryRow> = if phrases.is_empty() {
        let mut rows: Vec<MemoryRow> = rows(core).filter(MemoryRow::is_live).collect();
        rows.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));
        rows
    } else {
        ranked(core, phrases)
            .into_iter()
            .map(|(row, _)| row)
            .collect()
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
        .filter(|row| filter.tags.iter().all(|tag| core.tags.has(tag, &row.id)))
        .filter(|row| {
            filter
                .entity
                .as_ref()
                .is_none_or(|scope| linked.contains(&row.id) || names(row, &scope.canonical))
        })
        .collect();
    let total = matching.len();
    let page = matching
        .iter()
        .skip(offset)
        .take(limit)
        .map(MemoryRow::to_memory)
        .collect();
    Ok((total, page))
}

/// What `Memories::unannotated_page` selects: live, unsuperseded memories
/// that are not a dialog or a skeleton and have neither a triple part nor an
/// entity link, newest first, ties by id descending.
pub(crate) fn unannotated_page(
    tables: &EngineTables,
    limit: usize,
) -> Result<(usize, Vec<UnannotatedMemory>)> {
    let core = core_ref(tables)?;
    let mut rows: Vec<MemoryRow> = rows(core)
        .filter(|row| row.is_live())
        .filter(|row| !matches!(row.category.as_str(), "dialog" | "skeleton"))
        .filter(|row| row.subject.is_none() && row.predicate.is_none() && row.object.is_none())
        .filter(|row| !super::graph::has_links(core, &row.id))
        .collect();
    rows.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));
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

/// Up to `limit` memories after `(since, since_id)` on `(updated_at, id)`,
/// oldest first, skipping those `exclude_node` wrote, as the sync feed's
/// wire records: tags parsed or `[]`, metadata parsed or `{}`.
pub(crate) fn memories_after(
    tables: &EngineTables,
    since: &str,
    since_id: &str,
    exclude_node: Option<&str>,
    limit: usize,
) -> Result<Vec<Value>> {
    let mut found: Vec<MemoryRow> = rows(core_ref(tables)?)
        .filter(|row| (row.updated_at.as_str(), row.id.as_str()) > (since, since_id))
        .filter(|row| super::graph::not_excluded(row.node_id.as_deref(), exclude_node))
        .collect();
    found.sort_by(|a, b| (&a.updated_at, &a.id).cmp(&(&b.updated_at, &b.id)));
    Ok(found
        .into_iter()
        .take(limit)
        .map(|row| {
            serde_json::json!({
                "id": row.id,
                "content": row.content,
                "category": row.category,
                "tags": serde_json::from_str::<Value>(&row.tags).unwrap_or_else(|_| serde_json::json!([])),
                "source": row.source,
                "metadata": serde_json::from_str::<Value>(&row.metadata).unwrap_or_else(|_| serde_json::json!({})),
                "created_at": row.created_at,
                "updated_at": row.updated_at,
                "capture_id": row.capture_id,
                "node_id": row.node_id,
                "client": row.client,
                "accessed_at": row.accessed_at,
                "access_count": row.access_count,
                "decay_rate": row.decay_rate,
                "vitality": row.vitality,
                "base_weight": row.base_weight,
                "status": row.status,
                "memory_type": row.memory_type,
                "source_capture_id": row.source_capture_id,
                "subject": row.subject,
                "predicate": row.predicate,
                "object": row.object,
                "superseded_by": row.superseded_by,
                "deleted_at": row.deleted_at,
                "sensitive": row.sensitive,
                "remind_at": row.remind_at,
            })
        })
        .collect())
}

/// Rebuild the derived indexes from the rows.
pub(crate) fn rebuild(tables: &mut EngineTables) -> Result<()> {
    let core = core_mut(tables)?;
    let (search, tags) = index_all(&core.memories);
    core.search = search;
    core.tags = tags;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_multimodal_db_engine::fulltext::Query;

    const T1: &str = "2026-09-26T00:00:00+00:00";
    const T2: &str = "2026-09-27T00:00:00+00:00";

    fn hits(tables: &EngineTables, term: &str) -> Vec<String> {
        let mut ids: Vec<String> = core_ref(tables)
            .unwrap()
            .search
            .search(&Query::any_of([term]))
            .into_iter()
            .map(|hit| hit.key)
            .collect();
        ids.sort();
        ids
    }

    fn outbox_ops(tables: &EngineTables) -> Vec<(String, String)> {
        outbox::entries(core_ref(tables).unwrap())
            .into_iter()
            .map(|e| (e.memory_id, e.operation))
            .collect()
    }

    #[test]
    fn the_indexes_follow_a_memory_through_update_and_delete() {
        let mut tables = EngineTables::open_temporary().unwrap();
        let mut new = NewMemory::new("a", "quokka", T1);
        new.tags = vec!["red".to_string()];
        insert(&mut tables, &new).unwrap();
        assert_eq!(hits(&tables, "quokka"), ["a"]);
        assert!(core_ref(&tables).unwrap().tags.has("red", "a"));

        set_merged(&mut tables, "a", "wombat", 2, &["blue".to_string()], T2).unwrap();
        assert!(hits(&tables, "quokka").is_empty());
        assert_eq!(hits(&tables, "wombat"), ["a"]);
        let core = core_ref(&tables).unwrap();
        assert!(!core.tags.has("red", "a"));
        assert!(core.tags.has("blue", "a"));

        write(&mut tables, "a", Origin::Local, |_| Ok(Edit::Delete)).unwrap();
        assert!(hits(&tables, "wombat").is_empty());
        assert!(!core_ref(&tables).unwrap().tags.has("blue", "a"));
    }

    #[test]
    fn tags_are_what_json_each_yields() {
        let row = |tags: &str| MemoryRow {
            tags: tags.to_string(),
            ..MemoryRow::from_new(&NewMemory::new("a", "x", T1))
        };
        assert_eq!(row(r#"["a", 1, "b"]"#).tag_values(), ["a", "b"]);
        assert_eq!(row(r#""solo""#).tag_values(), ["solo"]);
        assert_eq!(row(r#"{"k": "v"}"#).tag_values(), ["v"]);
        assert!(row("not json").tag_values().is_empty());
    }

    #[test]
    fn only_local_edits_that_move_updated_at_are_queued() {
        let mut tables = EngineTables::open_temporary().unwrap();
        outbox::set_flag(&mut tables, "sync_enabled", "1").unwrap();
        insert(&mut tables, &NewMemory::new("local", "x", T1)).unwrap();
        upsert_synced(&mut tables, &NewMemory::new("synced", "y", T1)).unwrap();
        set_vitality(&mut tables, "local", 0.5, "active").unwrap();
        set_superseded_by(&mut tables, "local", "other", Some(T2)).unwrap();
        assert_eq!(
            outbox_ops(&tables),
            [
                ("local".to_string(), "insert".to_string()),
                ("local".to_string(), "update".to_string())
            ]
        );
    }

    #[test]
    fn nothing_is_queued_while_sync_is_off() {
        let mut tables = EngineTables::open_temporary().unwrap();
        insert(&mut tables, &NewMemory::new("a", "x", T1)).unwrap();
        assert!(outbox_ops(&tables).is_empty());
    }

    #[test]
    fn a_failed_edit_writes_nothing() {
        let mut tables = EngineTables::open_temporary().unwrap();
        insert(&mut tables, &NewMemory::new("a", "quokka", T1)).unwrap();
        let failed = update(&mut tables, "a", Origin::Local, |row| {
            row.content = "wombat".to_string();
            Err(engine_error("refused"))
        });
        assert!(failed.is_err());
        assert_eq!(hits(&tables, "quokka"), ["a"]);
        assert_eq!(
            row(core_ref(&tables).unwrap(), "a").unwrap().content,
            "quokka"
        );
    }

    #[test]
    fn the_ingest_marker_is_set_only_on_a_metadata_object() {
        let mut tables = EngineTables::open_temporary().unwrap();
        for (id, metadata) in [
            ("a", serde_json::json!({"k": 1})),
            ("b", serde_json::json!([1])),
        ] {
            let mut new = NewMemory::new(id, "x", T1);
            new.doc_id = Some("doc".to_string());
            new.metadata = metadata;
            insert(&mut tables, &new).unwrap();
        }
        assert_eq!(set_ingest_marker(&mut tables, "doc", "done").unwrap(), 2);
        let core = core_ref(&tables).unwrap();
        let a: Value = serde_json::from_str(&row(core, "a").unwrap().metadata).unwrap();
        assert_eq!(a, serde_json::json!({"k": 1, "ingest": "done"}));
        assert_eq!(row(core, "b").unwrap().metadata, "[1]");
    }

    #[test]
    fn memories_and_their_outbox_entries_survive_a_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_memories_reopen_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            outbox::set_flag(&mut tables, "sync_enabled", "1").unwrap();
            let mut new = NewMemory::new("a", "quokka", T1);
            new.tags = vec!["red".to_string()];
            insert(&mut tables, &new).unwrap();
        }
        let mut tables = crate::db::engine::reopen(&dir);
        assert_eq!(hits(&tables, "quokka"), ["a"]);
        assert!(core_ref(&tables).unwrap().tags.has("red", "a"));
        insert(&mut tables, &NewMemory::new("b", "x", T1)).unwrap();
        let ids: Vec<i64> = outbox::entries(core_ref(&tables).unwrap())
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids[1] > ids[0], "outbox ids keep rising: {ids:?}");
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
