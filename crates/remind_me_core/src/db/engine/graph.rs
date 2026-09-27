//! The knowledge graph on the engine core (ADR-0023, core PR 3a, built
//! dark): `entities`, `entity_relations` and the `memory_entities` mention
//! links. [`crate::db::entities`] calls these when its store carries the
//! core.
//!
//! A graph write and the outbox entry recording it commit as one journal
//! batch, with the payload shapes `db::derived` builds with `json_object`
//! on SQLite.

use super::core::{Change, CoreTables};
use super::memories::{self, MemoryRow};
use super::outbox;
use super::{core_ref, engine_error, engine_id, ensure_same_id, pair_engine_id, EngineTables};
use crate::db::derived::Origin;
use crate::db::entities::{EntitySyncView, RelationRow, StoredRelation};
use crate::db::Result;
use crate::entity::{Entity, EntityFact, EntityLinkedMemory, EntityListItem, RelationEdge};
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Index marker: a record by the string its lookups start from.
pub struct ByKey;
/// Slot marker: unused, always 0.
pub struct Slot;

pub(crate) type EntityTable = GenericMmapStore<EntityRecord, ByKey, Slot>;
pub(crate) type LinkTable = GenericMmapStore<LinkRecord, ByKey, Slot>;
pub(crate) type RelationTable = GenericMmapStore<RelationRecord, ByKey, Slot>;

/// One `entities` row, keyed by its id. Aliases stay JSON text, as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityRecord {
    engine_id: Uuid,
    id: String,
    name: String,
    kind: Option<String>,
    aliases: String,
    created_at: String,
    updated_at: String,
    node_id: Option<String>,
    slot: i64,
}

/// One `memory_entities` row, keyed by its (memory, entity) pair and
/// indexed by entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkRecord {
    engine_id: Uuid,
    memory_id: String,
    entity_id: String,
    created_at: String,
    slot: i64,
}

/// One `entity_relations` row, keyed by its id and indexed by subject.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationRecord {
    engine_id: Uuid,
    id: String,
    subject_entity_id: String,
    relation: String,
    object_entity_id: String,
    created_at: String,
    updated_at: String,
    node_id: Option<String>,
    slot: i64,
}

macro_rules! record {
    ($record:ty, $tag:literal, $key:ident) => {
        impl Record for $record {
            type Id = Uuid;
            fn id(&self) -> Uuid {
                self.engine_id
            }
        }
        impl SchemaTag for $record {
            const SCHEMA_TAG: &'static str = $tag;
        }
        impl IndexedField<ByKey> for $record {
            type IndexValue = String;
            fn indexed_value(&self) -> &String {
                &self.$key
            }
        }
        impl ScannableField<Slot> for $record {
            type ScanValue = i64;
            fn scannable_value(&self) -> i64 {
                self.slot
            }
            fn set_scannable_value(&mut self, value: i64) {
                self.slot = value;
            }
        }
    };
}

record!(EntityRecord, "rusty_remind_me::node::EntityRecord@1", name);
record!(LinkRecord, "rusty_remind_me::node::LinkRecord@1", entity_id);
record!(
    RelationRecord,
    "rusty_remind_me::node::RelationRecord@1",
    subject_entity_id
);

/// `aliases` as the JSON array the column holds.
fn aliases_json(aliases: &[String]) -> String {
    serde_json::to_string(aliases).unwrap_or_else(|_| "[]".to_string())
}

impl EntityRecord {
    fn new(entity: &Entity, node_id: Option<&str>) -> Self {
        Self {
            engine_id: engine_id(&entity.id),
            id: entity.id.clone(),
            name: entity.name.clone(),
            kind: entity.kind.clone(),
            aliases: aliases_json(&entity.aliases),
            created_at: entity.created_at.clone(),
            updated_at: entity.updated_at.clone(),
            node_id: node_id.map(str::to_string),
            slot: 0,
        }
    }

    fn to_entity(&self) -> Entity {
        Entity {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind.clone(),
            aliases: serde_json::from_str(&self.aliases).unwrap_or_default(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }

    /// The outbox payload `db::derived::queue_entity` builds.
    fn payload(&self) -> String {
        json!({
            "record_type": "entity",
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "aliases": self.aliases,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "node_id": self.node_id,
        })
        .to_string()
    }
}

impl LinkRecord {
    fn new(memory_id: &str, entity_id: &str, created_at: &str) -> Self {
        Self {
            engine_id: pair_engine_id(memory_id, entity_id),
            memory_id: memory_id.to_string(),
            entity_id: entity_id.to_string(),
            created_at: created_at.to_string(),
            slot: 0,
        }
    }

    /// The outbox payload `db::derived::queue_link` builds.
    fn payload(&self) -> String {
        json!({
            "record_type": "memory_entity",
            "id": format!("{}|{}", self.memory_id, self.entity_id),
            "memory_id": self.memory_id,
            "entity_id": self.entity_id,
            "created_at": self.created_at,
        })
        .to_string()
    }
}

impl RelationRecord {
    fn new(row: &RelationRow<'_>) -> Self {
        Self {
            engine_id: engine_id(row.id),
            id: row.id.to_string(),
            subject_entity_id: row.subject_entity_id.to_string(),
            relation: row.relation.to_string(),
            object_entity_id: row.object_entity_id.to_string(),
            created_at: row.created_at.to_string(),
            updated_at: row.updated_at.to_string(),
            node_id: row.node_id.map(str::to_string),
            slot: 0,
        }
    }

    /// The outbox payload `db::derived::queue_relation` builds.
    fn payload(&self) -> String {
        json!({
            "record_type": "entity_relation",
            "id": self.id,
            "subject_entity_id": self.subject_entity_id,
            "relation": self.relation,
            "object_entity_id": self.object_entity_id,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "node_id": self.node_id,
        })
        .to_string()
    }
}

// --- reads ----------------------------------------------------------------

fn entity(core: &CoreTables, id: &str) -> Option<EntityRecord> {
    core.entities.get(engine_id(id)).filter(|e| e.id == id)
}

fn entities(core: &CoreTables) -> Vec<EntityRecord> {
    core.entities
        .all_ids()
        .into_iter()
        .filter_map(|id| core.entities.get(id))
        .collect()
}

fn links(core: &CoreTables) -> Vec<LinkRecord> {
    core.links
        .all_ids()
        .into_iter()
        .filter_map(|id| core.links.get(id))
        .collect()
}

fn links_to(core: &CoreTables, entity_id: &str) -> Vec<LinkRecord> {
    FilterEq::<LinkRecord, ByKey>::filter_eq(&core.links, &entity_id.to_string())
        .into_iter()
        .filter_map(|id| core.links.get(id))
        .filter(|l| l.entity_id == entity_id)
        .collect()
}

fn relations(core: &CoreTables) -> Vec<RelationRecord> {
    core.relations
        .all_ids()
        .into_iter()
        .filter_map(|id| core.relations.get(id))
        .collect()
}

pub(crate) fn get(tables: &EngineTables, id: &str) -> Result<Option<Entity>> {
    Ok(entity(core_ref(tables)?, id).map(|e| e.to_entity()))
}

pub(crate) fn exists(tables: &EngineTables, id: &str) -> Result<bool> {
    Ok(entity(core_ref(tables)?, id).is_some())
}

/// Every entity, oldest first, ties by id.
pub(crate) fn all_oldest_first(tables: &EngineTables) -> Result<Vec<Entity>> {
    let mut all = entities(core_ref(tables)?);
    all.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    Ok(all.iter().map(EntityRecord::to_entity).collect())
}

/// Every entity, by id: the engine's "storage order".
pub(crate) fn all(tables: &EngineTables) -> Result<Vec<Entity>> {
    let mut all = entities(core_ref(tables)?);
    all.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(all.iter().map(EntityRecord::to_entity).collect())
}

pub(crate) fn count(tables: &EngineTables) -> Result<usize> {
    Ok(core_ref(tables)?.entities.all_ids().len())
}

/// A page of entities with their mention counts (every link, whether or not
/// its memory is stored), most-mentioned first, then by name, then id.
pub(crate) fn page_by_mentions(
    tables: &EngineTables,
    limit: usize,
    offset: usize,
) -> Result<Vec<EntityListItem>> {
    let core = core_ref(tables)?;
    let mut mentions: HashMap<String, i64> = HashMap::new();
    for link in links(core) {
        *mentions.entry(link.entity_id).or_insert(0) += 1;
    }
    let mut items: Vec<EntityListItem> = entities(core)
        .into_iter()
        .map(|e| EntityListItem {
            mention_count: mentions.get(&e.id).copied().unwrap_or(0),
            aliases: serde_json::from_str(&e.aliases).unwrap_or_default(),
            id: e.id,
            name: e.name,
            kind: e.kind,
            updated_at: e.updated_at,
        })
        .collect();
    items.sort_by(|a, b| {
        b.mention_count
            .cmp(&a.mention_count)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(items.into_iter().skip(offset).take(limit).collect())
}

pub(crate) fn links_oldest_first(tables: &EngineTables) -> Result<Vec<(String, String, String)>> {
    let mut all: Vec<(String, String, String)> = links(core_ref(tables)?)
        .into_iter()
        .map(|l| (l.created_at, l.memory_id, l.entity_id))
        .collect();
    all.sort();
    Ok(all
        .into_iter()
        .map(|(created_at, memory_id, entity_id)| (memory_id, entity_id, created_at))
        .collect())
}

/// The ids of the memories linked to `entity_id`.
pub(crate) fn linked_ids(tables: &EngineTables, entity_id: &str) -> Result<HashSet<String>> {
    Ok(links_to(core_ref(tables)?, entity_id)
        .into_iter()
        .map(|l| l.memory_id)
        .collect())
}

/// Whether memory `memory_id` has any entity link.
pub(crate) fn has_links(core: &CoreTables, memory_id: &str) -> bool {
    links(core).iter().any(|l| l.memory_id == memory_id)
}

/// Live, unsuperseded memories linked to `entity_id`, newest first (ties by
/// id, descending).
fn linked_rows(core: &CoreTables, entity_id: &str) -> Vec<MemoryRow> {
    let mut rows: Vec<MemoryRow> = links_to(core, entity_id)
        .into_iter()
        .filter_map(|l| memories::row(core, &l.memory_id))
        .filter(|row| row.deleted_at.is_none() && row.superseded_by.is_none())
        .collect();
    rows.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));
    rows
}

pub(crate) fn linked_memories(
    tables: &EngineTables,
    entity_id: &str,
    limit: usize,
) -> Result<Vec<EntityLinkedMemory>> {
    Ok(linked_rows(core_ref(tables)?, entity_id)
        .into_iter()
        .take(limit)
        .map(|row| EntityLinkedMemory {
            content_snippet: row.content.chars().take(300).collect(),
            id: row.id,
            category: row.category,
            created_at: row.created_at,
        })
        .collect())
}

pub(crate) fn linked_memory_count(tables: &EngineTables, entity_id: &str) -> Result<usize> {
    Ok(linked_rows(core_ref(tables)?, entity_id).len())
}

/// Live, unsuperseded memories whose subject or object, lowercased as
/// SQLite's ASCII-only `lower()` does, is `canonical`; newest first, ties by
/// id descending.
pub(crate) fn facts_naming(
    tables: &EngineTables,
    canonical: &str,
    limit: usize,
) -> Result<Vec<EntityFact>> {
    let names = |v: &Option<String>| {
        v.as_ref()
            .is_some_and(|v| v.to_ascii_lowercase() == canonical)
    };
    let mut rows: Vec<MemoryRow> = memories::rows(core_ref(tables)?)
        .filter(|row| row.deleted_at.is_none() && row.superseded_by.is_none())
        .filter(|row| names(&row.subject) || names(&row.object))
        .collect();
    rows.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));
    Ok(rows
        .into_iter()
        .take(limit)
        .map(|row| EntityFact {
            id: row.id,
            content: row.content,
            subject: row.subject,
            predicate: row.predicate,
            object: row.object,
            category: row.category,
            created_at: row.created_at,
        })
        .collect())
}

pub(crate) fn relations_oldest_first(tables: &EngineTables) -> Result<Vec<StoredRelation>> {
    let mut all = relations(core_ref(tables)?);
    all.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    Ok(all
        .into_iter()
        .map(|r| StoredRelation {
            id: r.id,
            subject_entity_id: r.subject_entity_id,
            relation: r.relation,
            object_entity_id: r.object_entity_id,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect())
}

/// Relations with either end in `entity_ids` (and labelled `relation`, when
/// given) whose ends are both stored, oldest first, ties by id.
pub(crate) fn relations_touching(
    tables: &EngineTables,
    entity_ids: &[String],
    relation: Option<&str>,
    hop: u32,
) -> Result<Vec<(String, RelationEdge)>> {
    let core = core_ref(tables)?;
    let wanted: HashSet<&str> = entity_ids.iter().map(String::as_str).collect();
    let mut found: Vec<RelationRecord> = relations(core)
        .into_iter()
        .filter(|r| {
            wanted.contains(r.subject_entity_id.as_str())
                || wanted.contains(r.object_entity_id.as_str())
        })
        .filter(|r| relation.is_none_or(|label| r.relation == label))
        .collect();
    found.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    Ok(found
        .into_iter()
        .filter_map(|r| {
            let subject = entity(core, &r.subject_entity_id)?;
            let object = entity(core, &r.object_entity_id)?;
            Some((
                r.id,
                RelationEdge {
                    subject_entity_id: r.subject_entity_id,
                    subject_name: subject.name,
                    subject_kind: subject.kind,
                    relation: r.relation,
                    object_entity_id: r.object_entity_id,
                    object_name: object.name,
                    object_kind: object.kind,
                    hop,
                },
            ))
        })
        .collect())
}

pub(crate) fn sync_view(tables: &EngineTables, id: &str) -> Result<Option<EntitySyncView>> {
    Ok(entity(core_ref(tables)?, id).map(|e| EntitySyncView {
        aliases: serde_json::from_str(&e.aliases).unwrap_or_default(),
        updated_at: e.updated_at,
    }))
}

// --- writes ---------------------------------------------------------------

/// `change`, and an outbox entry for `key` when sync is enabled, as one
/// batch.
fn commit_queued(
    tables: &mut EngineTables,
    mut changes: Vec<Change>,
    queued: Option<(&str, &str, String)>,
) -> Result<()> {
    if let Some((key, operation, payload)) = queued {
        if outbox::sync_enabled(core_ref(tables)?) {
            changes.push(outbox::entry(tables, key, operation, payload)?);
        }
    }
    tables.commit(changes)
}

fn put_entity(record: EntityRecord) -> Change {
    Change::Entity(record.engine_id, Some(Box::new(record)))
}

/// The stored entity `id`, refusing a different id at its engine id.
fn stored_entity(core: &CoreTables, id: &str) -> Result<Option<EntityRecord>> {
    let Some(record) = core.entities.get(engine_id(id)) else {
        return Ok(None);
    };
    ensure_same_id(&record.id, id)?;
    Ok(Some(record))
}

/// Insert `entity`, made here. A taken id is an error.
pub(crate) fn insert(tables: &mut EngineTables, new: &Entity, node_id: Option<&str>) -> Result<()> {
    if stored_entity(core_ref(tables)?, &new.id)?.is_some() {
        return Err(engine_error(format!("entity {:?} already exists", new.id)));
    }
    let record = EntityRecord::new(new, node_id);
    let payload = record.payload();
    commit_queued(
        tables,
        vec![put_entity(record)],
        Some((&new.id, "insert", payload)),
    )
}

/// Apply `change` to entity `id` if it exists, queuing an `update` of it.
fn update(
    tables: &mut EngineTables,
    id: &str,
    queue: bool,
    change: impl FnOnce(&mut EntityRecord),
) -> Result<()> {
    let Some(mut record) = stored_entity(core_ref(tables)?, id)? else {
        return Ok(());
    };
    change(&mut record);
    let payload = record.payload();
    let queued = queue.then_some((id, "update", payload));
    commit_queued(tables, vec![put_entity(record)], queued)
}

pub(crate) fn set_kind_and_aliases(
    tables: &mut EngineTables,
    id: &str,
    kind: Option<&str>,
    aliases: &[String],
    updated_at: &str,
) -> Result<()> {
    update(tables, id, true, |e| {
        e.kind = kind.map(str::to_string);
        e.aliases = aliases_json(aliases);
        e.updated_at = updated_at.to_string();
    })
}

pub(crate) fn set_merged(
    tables: &mut EngineTables,
    id: &str,
    kind: Option<&str>,
    aliases: &[String],
    created_at: &str,
) -> Result<()> {
    update(tables, id, true, |e| {
        e.kind = kind.map(str::to_string);
        e.aliases = aliases_json(aliases);
        e.created_at = created_at.to_string();
    })
}

/// Replace `id`'s aliases without stamping `updated_at`. Never queued.
pub(crate) fn set_aliases(tables: &mut EngineTables, id: &str, aliases: &[String]) -> Result<()> {
    update(tables, id, false, |e| e.aliases = aliases_json(aliases))
}

/// Rename entity `from` to `to`, queuing an update of `to`. As the SQL's
/// `UPDATE … SET id`, a missing `from` does nothing, and a taken `to` is an
/// error.
pub(crate) fn rename(tables: &mut EngineTables, from: &str, to: &str) -> Result<()> {
    let core = core_ref(tables)?;
    let Some(mut record) = stored_entity(core, from)? else {
        return Ok(());
    };
    if stored_entity(core, to)?.is_some() {
        return Err(engine_error(format!("entity {to:?} already exists")));
    }
    let old = record.engine_id;
    record.id = to.to_string();
    record.engine_id = engine_id(to);
    let payload = record.payload();
    commit_queued(
        tables,
        vec![Change::Entity(old, None), put_entity(record)],
        Some((to, "update", payload)),
    )
}

pub(crate) fn delete(tables: &mut EngineTables, id: &str) -> Result<()> {
    if stored_entity(core_ref(tables)?, id)?.is_none() {
        return Ok(());
    }
    tables.commit(vec![Change::Entity(engine_id(id), None)])
}

/// Repoint every link and relation end from `from` to `to`, dropping a
/// link `to` already has: `UPDATE OR IGNORE` then `DELETE`, in one batch.
pub(crate) fn repoint(tables: &mut EngineTables, from: &str, to: &str) -> Result<()> {
    let core = core_ref(tables)?;
    let mut changes = Vec::new();
    for link in links_to(core, from) {
        changes.push(Change::Link(link.engine_id, None));
        let moved = LinkRecord::new(&link.memory_id, to, &link.created_at);
        let taken = core.links.get(moved.engine_id).is_some();
        if !taken {
            changes.push(Change::Link(moved.engine_id, Some(moved)));
        }
    }
    for mut relation in relations(core) {
        if relation.subject_entity_id != from && relation.object_entity_id != from {
            continue;
        }
        if relation.subject_entity_id == from {
            relation.subject_entity_id = to.to_string();
        }
        if relation.object_entity_id == from {
            relation.object_entity_id = to.to_string();
        }
        changes.push(Change::Relation(
            relation.engine_id,
            Some(Box::new(relation)),
        ));
    }
    tables.commit(changes)
}

/// Record that `memory_id` mentions `entity_id`, unless it already does.
/// A new local link is queued. Whether it was new.
pub(crate) fn link(
    tables: &mut EngineTables,
    memory_id: &str,
    entity_id: &str,
    created_at: &str,
    origin: Origin,
) -> Result<bool> {
    let record = LinkRecord::new(memory_id, entity_id, created_at);
    if core_ref(tables)?.links.get(record.engine_id).is_some() {
        return Ok(false);
    }
    let queued = (origin == Origin::Local).then(|| (memory_id, "insert", record.payload()));
    commit_queued(
        tables,
        vec![Change::Link(record.engine_id, Some(record))],
        queued,
    )?;
    Ok(true)
}

/// Remove every link from memory `memory_id`: part of deleting a memory.
pub(crate) fn unlink_memory(tables: &mut EngineTables, memory_id: &str) -> Result<()> {
    let changes = links(core_ref(tables)?)
        .into_iter()
        .filter(|l| l.memory_id == memory_id)
        .map(|l| Change::Link(l.engine_id, None))
        .collect();
    tables.commit(changes)
}

/// Insert a relation unless its id is taken. A new local one is queued.
/// Whether it was new.
pub(crate) fn insert_relation_or_ignore(
    tables: &mut EngineTables,
    row: &RelationRow<'_>,
    origin: Origin,
) -> Result<bool> {
    let record = RelationRecord::new(row);
    if core_ref(tables)?.relations.get(record.engine_id).is_some() {
        return Ok(false);
    }
    let queued = (origin == Origin::Local).then(|| (row.id, "insert", record.payload()));
    commit_queued(
        tables,
        vec![Change::Relation(record.engine_id, Some(Box::new(record)))],
        queued,
    )?;
    Ok(true)
}

/// Insert a peer's entity, or overwrite everything but the local
/// `created_at`. Never queued.
pub(crate) fn upsert_synced(
    tables: &mut EngineTables,
    new: &Entity,
    node_id: Option<&str>,
) -> Result<()> {
    let mut record = EntityRecord::new(new, node_id);
    if let Some(local) = stored_entity(core_ref(tables)?, &new.id)? {
        record.created_at = local.created_at;
    }
    tables.commit(vec![put_entity(record)])
}

/// Every entity and link as backfill entries (key and payload), entities
/// oldest first, then links oldest first.
pub(crate) fn backfill_entries(core: &CoreTables) -> Vec<(String, String)> {
    let mut all_entities = entities(core);
    all_entities.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    let mut all_links = links(core);
    all_links.sort_by(|a, b| {
        (&a.created_at, &a.memory_id, &a.entity_id).cmp(&(
            &b.created_at,
            &b.memory_id,
            &b.entity_id,
        ))
    });
    all_entities
        .iter()
        .map(|e| (e.id.clone(), e.payload()))
        .chain(all_links.iter().map(|l| (l.memory_id.clone(), l.payload())))
        .collect()
}
