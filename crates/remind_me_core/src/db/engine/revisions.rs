//! Memory revisions on the engine (ADR-0023, phase 4g): one row per edit,
//! holding the tracked columns the edit replaced. [`crate::db::history`]
//! calls these when its store carries engine tables; its reads and writes
//! of `memories` itself stay on SQLite until memories move.
//!
//! A revision's id is an integer from the journal's `memory_revisions`
//! sequence, as SQLite's `INTEGER PRIMARY KEY` gives it: a caller passes it
//! back to revert, so it must never be issued twice.

use super::{engine_error, micros, EngineTables};
use crate::db::history::Tracked;
use crate::db::Result;
use crate::models::MemoryRevision;
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};

/// The journal sequence revision ids come from.
pub(crate) const SEQUENCE: &str = "memory_revisions";

/// Index marker: a revision by the memory it belongs to.
pub struct ByMemory;
/// Slot marker: when the edit happened, in µs since the epoch.
pub struct EditedAt;

pub(crate) type RevisionTable = GenericMmapStore<RevisionRecord, ByMemory, EditedAt>;

/// One `memory_revisions` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevisionRecord {
    id: i64,
    memory_id: String,
    content: String,
    category: String,
    tags: String,
    metadata: String,
    sensitive: Option<bool>,
    edited_at: String,
    edited_us: i64,
    revision_reason: Option<String>,
}

impl RevisionRecord {
    fn to_revision(&self) -> MemoryRevision {
        MemoryRevision {
            id: self.id,
            memory_id: self.memory_id.clone(),
            content: self.content.clone(),
            category: self.category.clone(),
            tags: self.tags.clone(),
            metadata: self.metadata.clone(),
            sensitive: self.sensitive,
            edited_at: self.edited_at.clone(),
            revision_reason: self.revision_reason.clone(),
        }
    }

    fn tracked(&self) -> Tracked {
        Tracked {
            content: self.content.clone(),
            category: self.category.clone(),
            tags: self.tags.clone(),
            metadata: self.metadata.clone(),
            sensitive: self.sensitive,
        }
    }
}

impl Record for RevisionRecord {
    type Id = i64;
    fn id(&self) -> i64 {
        self.id
    }
}

impl SchemaTag for RevisionRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::RevisionRecord@1";
}

impl IndexedField<ByMemory> for RevisionRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.memory_id
    }
}

impl ScannableField<EditedAt> for RevisionRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.edited_us
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.edited_us = value;
    }
}

/// Append a revision of `memory_id` holding `values`, under the next id.
pub(crate) fn insert(
    tables: &mut EngineTables,
    memory_id: &str,
    values: &Tracked,
    edited_at: &str,
    reason: Option<&str>,
) -> Result<()> {
    let id = tables.next_id(SEQUENCE)?;
    let record = RevisionRecord {
        id,
        memory_id: memory_id.to_string(),
        content: values.content.clone(),
        category: values.category.clone(),
        tags: values.tags.clone(),
        metadata: values.metadata.clone(),
        sensitive: values.sensitive,
        edited_at: edited_at.to_string(),
        edited_us: micros(edited_at),
        revision_reason: reason.map(str::to_string),
    };
    tables.revisions.insert(record).map_err(engine_error)
}

/// `memory_id`'s revisions, newest `edited_at` first as SQLite compares the
/// text, then highest id first, at most `limit`.
pub(crate) fn list(tables: &EngineTables, memory_id: &str, limit: usize) -> Vec<MemoryRevision> {
    let mut records = of_memory(tables, memory_id);
    records.sort_by(|a, b| (b.edited_at.as_str(), b.id).cmp(&(a.edited_at.as_str(), a.id)));
    records
        .iter()
        .take(limit)
        .map(RevisionRecord::to_revision)
        .collect()
}

/// The values revision `revision_id` holds, if it belongs to `memory_id`.
pub(crate) fn revision(
    tables: &EngineTables,
    memory_id: &str,
    revision_id: i64,
) -> Option<Tracked> {
    tables
        .revisions
        .get(revision_id)
        .filter(|r| r.memory_id == memory_id)
        .map(|r| r.tracked())
}

/// The highest revision id stored, or 0: the floor for the sequence when
/// the tables open.
pub(crate) fn max_id(table: &RevisionTable) -> u64 {
    table
        .all_ids()
        .into_iter()
        .max()
        .map_or(0, |id| u64::try_from(id).unwrap_or(0))
}

fn of_memory(tables: &EngineTables, memory_id: &str) -> Vec<RevisionRecord> {
    FilterEq::<RevisionRecord, ByMemory>::filter_eq(&tables.revisions, &memory_id.to_string())
        .into_iter()
        .filter_map(|id| tables.revisions.get(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(content: &str) -> Tracked {
        Tracked {
            content: content.to_string(),
            category: "fact".to_string(),
            tags: "[]".to_string(),
            metadata: "{}".to_string(),
            sensitive: None,
        }
    }

    #[test]
    fn revisions_and_their_ids_survive_a_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_revisions_reopen_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            insert(
                &mut tables,
                "m1",
                &values("a"),
                "2026-09-27T00:00:00+00:00",
                None,
            )
            .unwrap();
        }
        let mut tables = EngineTables::open(&dir).unwrap();
        insert(
            &mut tables,
            "m1",
            &values("b"),
            "2026-09-27T00:00:01+00:00",
            Some("why"),
        )
        .unwrap();
        let listed = list(&tables, "m1", 10);
        assert_eq!(listed.len(), 2);
        assert!(
            listed[0].id > listed[1].id,
            "newest first, ids never reused"
        );
        assert_eq!(listed[0].revision_reason.as_deref(), Some("why"));
        assert_eq!(revision(&tables, "m1", listed[1].id), Some(values("a")));
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
