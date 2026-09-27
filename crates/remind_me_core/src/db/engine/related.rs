//! Search expansion on the engine core (ADR-0023, core PR 3b, built dark):
//! `memory_associations` and the reads that find a result's relatives.
//! [`crate::db::related`] calls these when its store carries the core.

use super::core::{Change, CoreTables};
use super::memories::{self, MemoryRow};
use super::{core_ref, pair_engine_id, EngineTables};
use crate::db::related::{CoRetrieved, DocumentChunk, Relative};
use crate::db::Result;
use rusty_multimodal_db_engine::generic::query::{AllIds, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

/// Index marker: a pair by its first memory.
pub struct ByFirst;
/// Slot marker: the pair's weight.
pub struct Weight;

pub(crate) type AssociationTable = GenericMmapStore<AssociationRecord, ByFirst, Weight>;

/// One `memory_associations` row, keyed by its ordered pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssociationRecord {
    engine_id: Uuid,
    memory_id_a: String,
    memory_id_b: String,
    weight: i64,
    updated_at: String,
}

impl Record for AssociationRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for AssociationRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::AssociationRecord@1";
}

impl IndexedField<ByFirst> for AssociationRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.memory_id_a
    }
}

impl ScannableField<Weight> for AssociationRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.weight
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.weight = value;
    }
}

fn associations(core: &CoreTables) -> Vec<AssociationRecord> {
    core.associations
        .all_ids()
        .into_iter()
        .filter_map(|id| core.associations.get(id))
        .collect()
}

/// A new pair at weight 1, or a known one gaining 1 up to `max_weight`:
/// the SQL's upsert with `MIN(weight + 1, ?)`.
pub(crate) fn bump_pair(
    tables: &mut EngineTables,
    a: &str,
    b: &str,
    now: &str,
    max_weight: i64,
) -> Result<()> {
    let engine_id = pair_engine_id(a, b);
    let weight = match core_ref(tables)?.associations.get(engine_id) {
        Some(known) => (known.weight + 1).min(max_weight),
        None => 1,
    };
    let record = AssociationRecord {
        engine_id,
        memory_id_a: a.to_string(),
        memory_id_b: b.to_string(),
        weight,
        updated_at: now.to_string(),
    };
    tables.commit(vec![Change::Association(engine_id, Some(record))])
}

/// Remove every pair `memory_id` is part of: part of deleting a memory.
pub(crate) fn unlink_memory(tables: &mut EngineTables, memory_id: &str) -> Result<()> {
    let changes = associations(core_ref(tables)?)
        .into_iter()
        .filter(|p| p.memory_id_a == memory_id || p.memory_id_b == memory_id)
        .map(|p| Change::Association(p.engine_id, None))
        .collect();
    tables.commit(changes)
}

pub(crate) fn relative(row: &MemoryRow) -> Relative {
    Relative {
        id: row.id.clone(),
        content: row.content.clone(),
        category: row.category.clone(),
        created_at: row.created_at.clone(),
    }
}

fn live(core: &CoreTables, id: &str) -> Option<MemoryRow> {
    memories::row(core, id).filter(|row| row.deleted_at.is_none() && row.superseded_by.is_none())
}

/// Live chunks of `doc_id` with `chunk_index` in `from..=to`, in order, ties
/// by id.
pub(crate) fn document_window(
    tables: &EngineTables,
    doc_id: &str,
    from: i64,
    to: i64,
) -> Result<Vec<DocumentChunk>> {
    let mut rows: Vec<MemoryRow> = memories::rows(core_ref(tables)?)
        .filter(|row| row.deleted_at.is_none() && row.superseded_by.is_none())
        .filter(|row| row.doc_id.as_deref() == Some(doc_id))
        .filter(|row| row.chunk_index.is_some_and(|i| (from..=to).contains(&i)))
        .collect();
    rows.sort_by(|a, b| (a.chunk_index, &a.id).cmp(&(b.chunk_index, &b.id)));
    Ok(rows
        .iter()
        .map(|row| DocumentChunk {
            memory: relative(row),
            doc_id: row.doc_id.clone(),
            chunk_index: row.chunk_index,
        })
        .collect())
}

/// Live memories paired with any of `seed_ids`, one row per side a pair is
/// read from (the SQL's `UNION ALL`), strongest first, then newest, then id.
pub(crate) fn co_retrieved(tables: &EngineTables, seed_ids: &[String]) -> Result<Vec<CoRetrieved>> {
    let core = core_ref(tables)?;
    let seeds: HashSet<&str> = seed_ids.iter().map(String::as_str).collect();
    let mut found: Vec<(i64, MemoryRow)> = Vec::new();
    for pair in associations(core) {
        for (seed, other) in [
            (&pair.memory_id_a, &pair.memory_id_b),
            (&pair.memory_id_b, &pair.memory_id_a),
        ] {
            if seeds.contains(seed.as_str()) {
                if let Some(row) = live(core, other) {
                    found.push((pair.weight, row));
                }
            }
        }
    }
    found.sort_by(|(wa, a), (wb, b)| {
        wb.cmp(wa)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(found
        .iter()
        .map(|(weight, row)| CoRetrieved {
            memory: relative(row),
            weight: *weight,
        })
        .collect())
}
