//! A whole hub's rows, read from a retired backend, for
//! [`MultimodalHubStore::create_from_snapshot`](super::MultimodalHubStore::create_from_snapshot)
//! (ADR-0021, phase 3).
//!
//! Source-neutral: the SQLite and Postgres readers in [`crate::import`]
//! both produce one. Rows reuse the wire record types, plus the two
//! columns that never cross the wire: `origin_node`, which pull's
//! `exclude_node` filters on, and `hub_seq`, which every node's cursor
//! holds. Both are carried over exactly. A copy that renumbered `hub_seq`
//! would make every node skip every row up to its old cursor (decision 7).

use super::keys;
use super::rows::{EntityRow, LinkRow, MemoryRow, RelationRow};
use crate::record::{EntityRecord, EntityRelationRecord, LinkRecord, MemoryRecord};
use crate::store::{StoreError, StoreResult};
use std::collections::HashSet;
use uuid::Uuid;

/// A memory with the two hub-only columns.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotMemory {
    pub record: MemoryRecord,
    pub origin_node: Option<String>,
    pub hub_seq: i64,
}

/// A row the engine cannot store, or the reader could not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub table: &'static str,
    pub id: String,
    pub reason: String,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {:?}: {}", self.table, self.id, self.reason)
    }
}

/// Every row of a hub, ready to write.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub memories: Vec<SnapshotMemory>,
    pub entities: Vec<(EntityRecord, Option<String>)>,
    pub links: Vec<LinkRecord>,
    pub relations: Vec<(EntityRelationRecord, Option<String>)>,
    /// The highest `hub_seq` the source ever issued, where it knows one
    /// beyond its rows (a Postgres sequence's `last_value`). A row holding
    /// a higher number may have been deleted since; the copy must not
    /// issue it again.
    pub seq_high_water: i64,
    /// Rows the reader found but could not read, such as an unparseable
    /// timestamp. Reported with [`Self::validate`]'s findings.
    pub unreadable: Vec<Rejected>,
}

/// The engine rows a snapshot becomes.
pub(super) struct Rows {
    pub memories: Vec<MemoryRow>,
    pub entities: Vec<EntityRow>,
    pub links: Vec<LinkRow>,
    pub relations: Vec<RelationRow>,
}

/// Give every memory a `hub_seq`: its own where the source had one, and
/// for the rest the next numbers above everything issued, in
/// `(updated_at, id)` order. The retired SQL stores' `migrate` backfilled
/// in that order, so a copy of a never-migrated database numbers it as a
/// migration would have.
pub fn assign_missing_seq(
    rows: Vec<(MemoryRecord, Option<String>, Option<i64>)>,
    high_water: i64,
) -> Vec<SnapshotMemory> {
    let mut next = rows
        .iter()
        .filter_map(|(_, _, seq)| *seq)
        .fold(high_water, i64::max);
    let (mut numbered, mut missing): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|(_, _, seq)| seq.is_some());
    missing.sort_by(|a, b| {
        (a.0.updated_at.as_str(), a.0.id.as_str()).cmp(&(b.0.updated_at.as_str(), b.0.id.as_str()))
    });
    for row in &mut missing {
        next += 1;
        row.2 = Some(next);
    }
    numbered.extend(missing);
    numbered
        .into_iter()
        .map(|(record, origin_node, hub_seq)| SnapshotMemory {
            record,
            origin_node,
            hub_seq: hub_seq.unwrap_or_default(),
        })
        .collect()
}

fn reject(table: &'static str, id: &str, err: StoreError) -> Rejected {
    Rejected {
        table,
        id: id.to_string(),
        reason: err.0,
    }
}

impl Snapshot {
    /// Every row the engine would refuse, plus every row the reader could
    /// not read: ids over the 64-byte cap, empty or holding NUL, and links
    /// naming such an id. Empty means the snapshot copies whole.
    pub fn validate(&self) -> Vec<Rejected> {
        let mut out = self.unreadable.clone();
        for m in &self.memories {
            if let Err(e) = keys::check_id("memory id", &m.record.id) {
                out.push(reject("memories", &m.record.id, e));
            }
        }
        for (e, _) in &self.entities {
            if let Err(err) = keys::check_id("entity id", &e.id) {
                out.push(reject("entities", &e.id, err));
            }
        }
        for l in &self.links {
            let id = format!("{}|{}", l.memory_id, l.entity_id);
            let checked = keys::check_id("memory_id", &l.memory_id)
                .and_then(|()| keys::check_id("entity_id", &l.entity_id));
            if let Err(e) = checked {
                out.push(reject("memory_entities", &id, e));
            }
        }
        for (r, _) in &self.relations {
            if let Err(e) = keys::check_id("relation id", &r.id) {
                out.push(reject("entity_relations", &r.id, e));
            }
        }
        out
    }

    /// This snapshot without the rows [`Self::validate`] rejects, and what
    /// was dropped. For the copy tool's `--drop-invalid`.
    pub fn without_rejected(mut self) -> (Self, Vec<Rejected>) {
        let rejected = self.validate();
        let ok = |id: &str| keys::check_id("id", id).is_ok();
        self.memories.retain(|m| ok(&m.record.id));
        self.entities.retain(|(e, _)| ok(&e.id));
        self.links.retain(|l| ok(&l.memory_id) && ok(&l.entity_id));
        self.relations.retain(|(r, _)| ok(&r.id));
        self.unreadable.clear();
        (self, rejected)
    }

    /// The `hub_seq` the copy records as issued: the highest copied or the
    /// source's high-water mark, whichever is higher.
    pub fn seq_floor(&self) -> i64 {
        self.memories
            .iter()
            .map(|m| m.hub_seq)
            .fold(self.seq_high_water, i64::max)
    }

    /// The engine rows, refusing two rows that share an engine id.
    pub(super) fn rows(&self) -> StoreResult<Rows> {
        let memories = self
            .memories
            .iter()
            .map(|m| MemoryRow::new(&m.record, m.origin_node.as_deref(), m.hub_seq))
            .collect::<StoreResult<Vec<_>>>()?;
        let entities = self
            .entities
            .iter()
            .map(|(e, origin)| EntityRow::new(e, origin.as_deref()))
            .collect::<StoreResult<Vec<_>>>()?;
        let links = self
            .links
            .iter()
            .map(LinkRow::new)
            .collect::<StoreResult<Vec<_>>>()?;
        let relations = self
            .relations
            .iter()
            .map(|(r, origin)| RelationRow::new(r, origin.as_deref()))
            .collect::<StoreResult<Vec<_>>>()?;
        unique("memories", memories.iter().map(|r| r.engine_id))?;
        unique("entities", entities.iter().map(|r| r.engine_id))?;
        unique("memory_entities", links.iter().map(|r| r.engine_id))?;
        unique("entity_relations", relations.iter().map(|r| r.engine_id))?;
        Ok(Rows {
            memories,
            entities,
            links,
            relations,
        })
    }
}

/// Refuse a table where two rows map to one engine id. The engine would
/// keep one silently.
fn unique(table: &str, ids: impl Iterator<Item = Uuid>) -> StoreResult<()> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(StoreError(format!(
                "two {table} rows map to engine id {id}; refusing to merge them"
            )));
        }
    }
    Ok(())
}
