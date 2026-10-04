//! `memory_references` on the engine core (schema v32): the URLs, issues,
//! commits, paths, handles and attachments a memory names.
//! [`crate::db::references`] calls these when its store carries the core.

use super::core::{Change, CoreTables};
use super::{core_ref, engine_error, engine_id, EngineTables};
use crate::db::references::{MemoryReference, NewReference};
use crate::db::Result;
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Index marker: a reference by the memory it belongs to.
pub struct ByMemory;
/// Slot marker: unused, always 0.
pub struct Slot;

pub(crate) type ReferenceTable = GenericMmapStore<ReferenceRecord, ByMemory, Slot>;

/// One `memory_references` row, keyed by its id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceRecord {
    engine_id: Uuid,
    id: String,
    memory_id: String,
    kind: String,
    value: String,
    label: Option<String>,
    created_at: String,
    slot: i64,
}

impl ReferenceRecord {
    fn new(row: &NewReference) -> Self {
        Self {
            engine_id: engine_id(&row.id),
            id: row.id.clone(),
            memory_id: row.memory_id.clone(),
            kind: row.kind.clone(),
            value: row.value.clone(),
            label: row.label.clone(),
            created_at: row.created_at.clone(),
            slot: 0,
        }
    }

    fn to_row(&self) -> MemoryReference {
        MemoryReference {
            id: self.id.clone(),
            memory_id: self.memory_id.clone(),
            kind: self.kind.clone(),
            value: self.value.clone(),
            label: self.label.clone(),
            created_at: self.created_at.clone(),
        }
    }
}

impl Record for ReferenceRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for ReferenceRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::ReferenceRecord@1";
}

impl IndexedField<ByMemory> for ReferenceRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.memory_id
    }
}

impl ScannableField<Slot> for ReferenceRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.slot
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.slot = value;
    }
}

/// The rows `keep` selects, oldest first, ties by id: the order the SQL
/// reads them in.
fn rows(core: &CoreTables, keep: impl Fn(&ReferenceRecord) -> bool) -> Vec<MemoryReference> {
    let mut found: Vec<ReferenceRecord> = core
        .references
        .all_ids()
        .into_iter()
        .filter_map(|id| core.references.get(id))
        .filter(|r| keep(r))
        .collect();
    found.sort_by(|a, b| {
        (&a.created_at, &a.kind, &a.value, &a.id).cmp(&(&b.created_at, &b.kind, &b.value, &b.id))
    });
    found.iter().map(ReferenceRecord::to_row).collect()
}

/// The records of memory `memory_id`, through the index.
fn of_memory(core: &CoreTables, memory_id: &str) -> Vec<ReferenceRecord> {
    FilterEq::<ReferenceRecord, ByMemory>::filter_eq(&core.references, &memory_id.to_string())
        .into_iter()
        .filter_map(|id| core.references.get(id))
        .filter(|r| r.memory_id == memory_id)
        .collect()
}

/// Insert `row`. An id already stored is an error, as the primary key
/// makes it on SQLite.
pub(crate) fn insert(tables: &mut EngineTables, row: &NewReference) -> Result<()> {
    let record = ReferenceRecord::new(row);
    if core_ref(tables)?.references.get(record.engine_id).is_some() {
        return Err(engine_error(format!(
            "reference {:?} already exists",
            row.id
        )));
    }
    let id = record.engine_id;
    tables.commit(vec![Change::Reference(id, Some(Box::new(record)))])
}

pub(crate) fn for_memory(tables: &EngineTables, memory_id: &str) -> Result<Vec<MemoryReference>> {
    let core = core_ref(tables)?;
    let mut found = of_memory(core, memory_id);
    found.sort_by(|a, b| {
        (&a.created_at, &a.kind, &a.value, &a.id).cmp(&(&b.created_at, &b.kind, &b.value, &b.id))
    });
    Ok(found.iter().map(ReferenceRecord::to_row).collect())
}

/// The references of every id in `memory_ids`, grouped by neither: one
/// ordered list (oldest first, ties by kind, value, then id), so a caller regroups by `memory_id`.
pub(crate) fn for_memories(
    tables: &EngineTables,
    memory_ids: &[String],
) -> Result<Vec<MemoryReference>> {
    let core = core_ref(tables)?;
    let mut found: Vec<ReferenceRecord> = memory_ids
        .iter()
        .flat_map(|id| of_memory(core, id))
        .collect();
    found.sort_by(|a, b| {
        (&a.created_at, &a.kind, &a.value, &a.id).cmp(&(&b.created_at, &b.kind, &b.value, &b.id))
    });
    Ok(found.iter().map(ReferenceRecord::to_row).collect())
}

/// Every reference of this `kind`, oldest first.
pub(crate) fn of_kind(tables: &EngineTables, kind: &str) -> Result<Vec<MemoryReference>> {
    Ok(rows(core_ref(tables)?, |r| r.kind == kind))
}

pub(crate) fn find(tables: &EngineTables, kind: &str, value: &str) -> Result<Vec<MemoryReference>> {
    Ok(rows(core_ref(tables)?, |r| {
        r.kind == kind && r.value == value
    }))
}

/// Remove every reference of `memory_id`. How many went.
pub(crate) fn delete_for_memory(tables: &mut EngineTables, memory_id: &str) -> Result<usize> {
    let changes: Vec<Change> = of_memory(core_ref(tables)?, memory_id)
        .into_iter()
        .map(|r| Change::Reference(r.engine_id, None))
        .collect();
    let count = changes.len();
    tables.commit(changes)?;
    Ok(count)
}
