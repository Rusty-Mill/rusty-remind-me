//! Embeddings on the engine core (ADR-0023, core PR 4a, built dark):
//! `vec_chunks`, keyed by (memory id, chunk index), and `embedding_meta`.
//! [`crate::db::vectors`] calls these when its store carries the core.

use super::core::{Change, CoreTables};
use super::memories::{self, MemoryRow};
use super::{core_ref, engine_id, ensure_same_id, pair_engine_id, EngineTables};
use crate::db::vectors::{ChunkVector, ConsolidationCandidate, Unembedded};
use crate::db::{Result, StoreError};
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

/// Index marker: a chunk by its memory.
pub struct ByMemory;
/// Slot marker: the chunk's index within its memory.
pub struct ChunkIx;
/// Index marker: an `embedding_meta` entry by its key.
pub struct ByKey;
/// Slot marker: unused, always 0.
pub struct Slot;

pub(crate) type ChunkTable = GenericMmapStore<ChunkRecord, ByMemory, ChunkIx>;
pub(crate) type MetaTable = GenericMmapStore<MetaRecord, ByKey, Slot>;

/// One `vec_chunks` row: a memory's chunk vector as little-endian f32
/// bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkRecord {
    engine_id: Uuid,
    memory_id: String,
    chunk_ix: i64,
    embedding: Vec<u8>,
}

impl Record for ChunkRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for ChunkRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::ChunkRecord@1";
}

impl IndexedField<ByMemory> for ChunkRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.memory_id
    }
}

impl ScannableField<ChunkIx> for ChunkRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.chunk_ix
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.chunk_ix = value;
    }
}

/// One `embedding_meta` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetaRecord {
    engine_id: Uuid,
    key: String,
    value: String,
    updated_at: String,
    slot: i64,
}

impl Record for MetaRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for MetaRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::EmbeddingMetaRecord@1";
}

impl IndexedField<ByKey> for MetaRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.key
    }
}

impl ScannableField<Slot> for MetaRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.slot
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.slot = value;
    }
}

/// The engine id of chunk `chunk_ix` of `memory_id`.
fn chunk_id(memory_id: &str, chunk_ix: i64) -> Uuid {
    pair_engine_id(memory_id, &chunk_ix.to_string())
}

/// Every chunk, in key order: by memory, then by chunk index.
fn chunks(core: &CoreTables) -> Vec<ChunkRecord> {
    let mut all: Vec<ChunkRecord> = core
        .chunks
        .all_ids()
        .into_iter()
        .filter_map(|id| core.chunks.get(id))
        .collect();
    all.sort_by(|a, b| (&a.memory_id, a.chunk_ix).cmp(&(&b.memory_id, b.chunk_ix)));
    all
}

/// The chunks of `memory_id`.
fn chunks_of(core: &CoreTables, memory_id: &str) -> Vec<ChunkRecord> {
    FilterEq::<ChunkRecord, ByMemory>::filter_eq(&core.chunks, &memory_id.to_string())
        .into_iter()
        .filter_map(|id| core.chunks.get(id))
        .filter(|c| c.memory_id == memory_id)
        .collect()
}

fn vector(chunk: ChunkRecord) -> ChunkVector {
    ChunkVector {
        memory_id: chunk.memory_id,
        embedding: chunk.embedding,
    }
}

fn live(row: &MemoryRow) -> bool {
    row.deleted_at.is_none() && row.superseded_by.is_none()
}

/// Store `embedding` as chunk `chunk_ix` of `memory_id`: `INSERT OR
/// REPLACE`.
pub(crate) fn put(
    tables: &mut EngineTables,
    memory_id: &str,
    chunk_ix: i64,
    embedding: &[u8],
) -> Result<()> {
    let id = chunk_id(memory_id, chunk_ix);
    let record = ChunkRecord {
        engine_id: id,
        memory_id: memory_id.to_string(),
        chunk_ix,
        embedding: embedding.to_vec(),
    };
    tables.commit(vec![Change::Chunk(id, Some(Box::new(record)))])
}

pub(crate) fn delete_for(tables: &mut EngineTables, memory_id: &str) -> Result<usize> {
    let doomed: Vec<Uuid> = chunks_of(core_ref(tables)?, memory_id)
        .into_iter()
        .map(|c| c.engine_id)
        .collect();
    let count = doomed.len();
    tables.commit(
        doomed
            .into_iter()
            .map(|id| Change::Chunk(id, None))
            .collect(),
    )?;
    Ok(count)
}

pub(crate) fn clear(tables: &mut EngineTables) -> Result<()> {
    let doomed = core_ref(tables)?.chunks.all_ids();
    tables.commit(
        doomed
            .into_iter()
            .map(|id| Change::Chunk(id, None))
            .collect(),
    )
}

pub(crate) fn chunk_count(tables: &EngineTables, memory_id: &str) -> Result<usize> {
    Ok(chunks_of(core_ref(tables)?, memory_id).len())
}

pub(crate) fn count(tables: &EngineTables) -> Result<usize> {
    Ok(core_ref(tables)?.chunks.all_ids().len())
}

/// The first chunk in key order, as the SQL's `ORDER BY memory_id,
/// chunk_ix LIMIT 1` picks it.
pub(crate) fn any_embedding(tables: &EngineTables) -> Result<Option<Vec<u8>>> {
    Ok(chunks(core_ref(tables)?)
        .into_iter()
        .next()
        .map(|c| c.embedding))
}

pub(crate) fn all(tables: &EngineTables) -> Result<Vec<ChunkVector>> {
    Ok(chunks(core_ref(tables)?).into_iter().map(vector).collect())
}

pub(crate) fn live_chunks(
    tables: &EngineTables,
    category: Option<&str>,
    among: Option<&[String]>,
) -> Result<Vec<ChunkVector>> {
    let core = core_ref(tables)?;
    let among: Option<HashSet<&str>> = among.map(|ids| ids.iter().map(String::as_str).collect());
    let keep = |memory_id: &str| {
        let Some(row) = memories::row(core, memory_id) else {
            return false;
        };
        live(&row)
            && category.is_none_or(|c| row.category == c)
            && among.as_ref().is_none_or(|ids| ids.contains(memory_id))
    };
    Ok(chunks(core)
        .into_iter()
        .filter(|c| keep(&c.memory_id))
        .map(vector)
        .collect())
}

pub(crate) fn unembedded(tables: &EngineTables) -> Result<Vec<Unembedded>> {
    let core = core_ref(tables)?;
    let embedded: HashSet<String> = chunks(core).into_iter().map(|c| c.memory_id).collect();
    let mut rows: Vec<MemoryRow> = memories::rows(core)
        .filter(|r| r.deleted_at.is_none() && !embedded.contains(&r.id))
        .collect();
    rows.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    Ok(rows
        .into_iter()
        .map(|r| Unembedded {
            id: r.id,
            content: r.content,
        })
        .collect())
}

/// Active, live memories with a first chunk, oldest first (ties by id), at
/// most `limit`.
pub(crate) fn consolidation_candidates(
    tables: &EngineTables,
    category: Option<&str>,
    limit: usize,
) -> Result<Vec<ConsolidationCandidate>> {
    let core = core_ref(tables)?;
    let mut rows: Vec<MemoryRow> = memories::rows(core)
        .filter(|r| r.status == "active" && live(r))
        .filter(|r| category.is_none_or(|c| r.category == c))
        .collect();
    rows.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    let mut candidates = Vec::new();
    for row in rows {
        if candidates.len() >= limit {
            break;
        }
        let Some(first) = core.chunks.get(chunk_id(&row.id, 0)) else {
            continue;
        };
        if first.memory_id != row.id {
            continue;
        }
        // The SQL reads `accessed_at` as text, and fails on a NULL.
        let accessed_at = row
            .accessed_at
            .clone()
            .ok_or_else(|| StoreError::Engine(format!("memory {:?} has no accessed_at", row.id)))?;
        candidates.push(ConsolidationCandidate {
            tags: serde_json::from_str(&row.tags).unwrap_or_default(),
            id: row.id,
            content: row.content,
            vitality: row.vitality,
            access_count: row.access_count,
            accessed_at,
            decay_rate: row.decay_rate,
            base_weight: row.base_weight,
            embedding: first.embedding,
        });
    }
    Ok(candidates)
}

/// Every `embedding_meta` key and value, by key.
pub(crate) fn meta(tables: &EngineTables) -> Result<Vec<(String, String)>> {
    let core = core_ref(tables)?;
    let mut entries: Vec<(String, String)> = core
        .embedding_meta
        .all_ids()
        .into_iter()
        .filter_map(|id| core.embedding_meta.get(id))
        .map(|m| (m.key, m.value))
        .collect();
    entries.sort();
    Ok(entries)
}

/// Set the value under `key`: the SQL's upsert.
pub(crate) fn set_meta(
    tables: &mut EngineTables,
    key: &str,
    value: &str,
    updated_at: &str,
) -> Result<()> {
    let id = engine_id(key);
    if let Some(stored) = core_ref(tables)?.embedding_meta.get(id) {
        ensure_same_id(&stored.key, key)?;
    }
    let record = MetaRecord {
        engine_id: id,
        key: key.to_string(),
        value: value.to_string(),
        updated_at: updated_at.to_string(),
        slot: 0,
    };
    tables.commit(vec![Change::EmbeddingMeta(id, Some(record))])
}
