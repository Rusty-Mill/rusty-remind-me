//! The memories core on the engine (ADR-0023, core PR 1): memories, the
//! sync outbox, its send markers and the sync flags, and the indexes derived
//! from the memories.
//!
//! Built dark: only [`EngineTables::open_with_core`] opens these, and only
//! the core's own tests call it, until the switch-on PR. A store whose
//! tables have no core keeps every one of these groups on SQLite.
//!
//! Writes that must land together travel as one journal batch (ADR-0023
//! §3b): a memory and the outbox entry recording it, a prune's entries and
//! markers. [`EngineTables::commit`] makes the batch durable, applies it to
//! the stores, and checkpoints; a crash in between is repaired at the next
//! open, which applies the batch again. Applying is idempotent: a put
//! inserts or replaces, a delete of a missing record is a no-op.

use super::memories::{self, MemoryRecord, MemorySearch, MemoryTable, TagIndex};
use super::outbox::{FlagRecord, FlagTable, OutboxRecord, OutboxTable, SendRecord, SendTable};
use super::{engine_error, open_core, EngineTables};
use crate::db::{Result, StoreError};
use rusty_multimodal_db_engine::generic::mmap_field::MmapFieldValue;
use rusty_multimodal_db_engine::generic::query::GetById;
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::{DeleteError, GenericMmapStore};
use rusty_multimodal_db_engine::journal::Batch;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;
use uuid::Uuid;

/// The core's stores and derived indexes.
pub(crate) struct CoreTables {
    pub(crate) memories: MemoryTable,
    pub(crate) outbox: OutboxTable,
    pub(crate) sends: SendTable,
    pub(crate) flags: FlagTable,
    /// `memories_fts`: derived from `memories` at open, never stored.
    pub(crate) search: MemorySearch,
    /// `memory_tags`: derived from `memories` at open, never stored.
    pub(crate) tags: TagIndex,
}

impl CoreTables {
    pub(super) fn open(dir: &Path) -> Result<Self> {
        let memories: MemoryTable = open_core(&dir.join("memories.mmap"))?;
        let (search, tags) = memories::index_all(&memories);
        Ok(Self {
            memories,
            outbox: open_core(&dir.join("sync_outbox.mmap"))?,
            sends: open_core(&dir.join("sync_sends.mmap"))?,
            flags: open_core(&dir.join("sync_flags.mmap"))?,
            search,
            tags,
        })
    }
}

/// One record a batch writes (`Some`) or removes (`None`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Change {
    Memory(Uuid, Option<Box<MemoryRecord>>),
    Outbox(i64, Option<OutboxRecord>),
    Send(Uuid, Option<SendRecord>),
    Flag(Uuid, Option<FlagRecord>),
}

/// The journal's names for the core stores. Pinned: a journal written by
/// one build is replayed by the next.
const MEMORIES: &str = "memories";
const OUTBOX: &str = "sync_outbox";
const SENDS: &str = "sync_sends";
const FLAGS: &str = "sync_flags";

/// `changes` as a journal batch: each key and record as JSON.
pub(super) fn encode(changes: &[Change]) -> Result<Batch> {
    let mut batch = Batch::default();
    for change in changes {
        match change {
            Change::Memory(id, record) => put_or_delete(&mut batch, MEMORIES, id, record)?,
            Change::Outbox(id, record) => put_or_delete(&mut batch, OUTBOX, id, record)?,
            Change::Send(id, record) => put_or_delete(&mut batch, SENDS, id, record)?,
            Change::Flag(id, record) => put_or_delete(&mut batch, FLAGS, id, record)?,
        }
    }
    Ok(batch)
}

fn put_or_delete<K: Serialize, R: Serialize>(
    batch: &mut Batch,
    store: &str,
    key: &K,
    record: &Option<R>,
) -> Result<()> {
    let key = serde_json::to_vec(key).map_err(engine_error)?;
    match record {
        Some(record) => {
            let value = serde_json::to_vec(record).map_err(engine_error)?;
            batch.put(store, key, value);
        }
        None => {
            batch.delete(store, key);
        }
    }
    Ok(())
}

/// A journal batch back as changes. A store this build does not know was
/// written by a newer one, and is refused rather than dropped.
pub(super) fn decode(batch: &Batch) -> Result<Vec<Change>> {
    batch
        .changes
        .iter()
        .map(|change| {
            let value = change.value.as_deref();
            match change.store.as_str() {
                MEMORIES => Ok(Change::Memory(key(&change.key)?, record(value)?)),
                OUTBOX => Ok(Change::Outbox(key(&change.key)?, record(value)?)),
                SENDS => Ok(Change::Send(key(&change.key)?, record(value)?)),
                FLAGS => Ok(Change::Flag(key(&change.key)?, record(value)?)),
                other => Err(StoreError::Engine(format!(
                    "the journal holds changes to {other:?}, which this build cannot apply"
                ))),
            }
        })
        .collect()
}

fn key<K: DeserializeOwned>(bytes: &[u8]) -> Result<K> {
    serde_json::from_slice(bytes).map_err(engine_error)
}

fn record<R: DeserializeOwned>(bytes: Option<&[u8]>) -> Result<Option<R>> {
    bytes
        .map(|b| serde_json::from_slice(b).map_err(engine_error))
        .transpose()
}

/// Apply one change to the stores, and keep the derived indexes in step.
pub(super) fn apply(core: &mut CoreTables, change: Change) -> Result<()> {
    match change {
        Change::Memory(id, record) => {
            if let Some(old) = core.memories.get(id) {
                memories::unindex(&mut core.search, &mut core.tags, &old.row);
            }
            match record {
                Some(record) => {
                    let row = record.row.clone();
                    put(&mut core.memories, *record)?;
                    memories::index(&mut core.search, &mut core.tags, &row);
                    Ok(())
                }
                None => remove(core.memories.delete(id)),
            }
        }
        Change::Outbox(id, record) => put_or_remove(&mut core.outbox, id, record),
        Change::Send(id, record) => put_or_remove(&mut core.sends, id, record),
        Change::Flag(id, record) => put_or_remove(&mut core.flags, id, record),
    }
}

fn put_or_remove<R, Index, Slot>(
    table: &mut GenericMmapStore<R, Index, Slot>,
    id: R::Id,
    record: Option<R>,
) -> Result<()>
where
    R: Record
        + IndexedField<Index>
        + ScannableField<Slot>
        + Clone
        + Serialize
        + DeserializeOwned
        + SchemaTag,
    R::Id: MmapFieldValue + Serialize + DeserializeOwned + std::fmt::Debug,
    R::ScanValue: MmapFieldValue,
{
    match record {
        Some(record) => put(table, record),
        None => remove(table.delete(id)),
    }
}

/// Insert `record`, or replace the one stored under its id.
fn put<R, Index, Slot>(table: &mut GenericMmapStore<R, Index, Slot>, record: R) -> Result<()>
where
    R: Record
        + IndexedField<Index>
        + ScannableField<Slot>
        + Clone
        + Serialize
        + DeserializeOwned
        + SchemaTag,
    R::Id: MmapFieldValue + Serialize + DeserializeOwned + std::fmt::Debug,
    R::ScanValue: MmapFieldValue,
{
    if table.get(record.id()).is_some() {
        table.replace(record).map_err(engine_error)
    } else {
        table.insert(record).map_err(engine_error)
    }
}

/// A delete's result, with an already-missing record counted as deleted:
/// a replayed batch may delete what its first run already removed.
fn remove<Id: std::fmt::Debug>(result: std::result::Result<(), DeleteError<Id>>) -> Result<()>
where
    DeleteError<Id>: std::fmt::Display,
{
    match result {
        Ok(()) | Err(DeleteError::NotFound(_)) => Ok(()),
        Err(e) => Err(engine_error(e)),
    }
}

impl EngineTables {
    /// Make `changes` durable as one journal batch, apply them, and
    /// checkpoint.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if the tables have no core, or the journal or
    /// a store fails. A failure after the batch is durable leaves the
    /// tables refusing every later write until they are reopened, which
    /// applies the batch again: checkpointing past it would lose it.
    pub(crate) fn commit(&mut self, changes: Vec<Change>) -> Result<()> {
        self.ensure_writable()?;
        let Some(core) = self.core.as_mut() else {
            return Err(no_core());
        };
        if changes.is_empty() {
            return Ok(());
        }
        let batch = encode(&changes)?;
        self.journal.commit(&batch).map_err(engine_error)?;
        for change in changes {
            if let Err(e) = apply(core, change) {
                self.failed = true;
                return Err(e);
            }
        }
        self.journal.checkpoint().map_err(engine_error)
    }

    /// Apply the batches a journal handed back at open, then checkpoint.
    pub(super) fn replay(&mut self, batches: Vec<Batch>) -> Result<()> {
        let batches: Vec<Batch> = batches.into_iter().filter(|b| !b.is_empty()).collect();
        if batches.is_empty() {
            return Ok(());
        }
        let Some(core) = self.core.as_mut() else {
            return Err(StoreError::Engine(
                "the journal holds changes this build cannot apply without the memories core"
                    .to_string(),
            ));
        };
        for batch in &batches {
            for change in decode(batch)? {
                apply(core, change)?;
            }
        }
        self.journal.checkpoint().map_err(engine_error)
    }
}

/// The error for a core read or write on tables opened without the core.
pub(super) fn no_core() -> StoreError {
    StoreError::Engine("the memories core is not open on these tables".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::engine::memories::MemoryRow;
    use crate::db::memories::NewMemory;

    fn memory(id: &str) -> MemoryRecord {
        MemoryRecord::new(MemoryRow::from_new(&NewMemory::new(
            id,
            "quokka",
            "2026-09-27T00:00:00+00:00",
        )))
    }

    #[test]
    fn a_batch_round_trips_through_the_journal_encoding() {
        let changes = vec![
            Change::Memory(super::super::engine_id("a"), Some(Box::new(memory("a")))),
            Change::Outbox(7, None),
        ];
        assert_eq!(decode(&encode(&changes).unwrap()).unwrap(), changes);
    }

    #[test]
    fn a_batch_left_in_the_journal_is_applied_at_the_next_open() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_core_replay_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        {
            // A crash after the batch was durable and before it reached the
            // stores: commit to the journal alone.
            let mut tables = EngineTables::open_with_core(&dir).unwrap();
            let batch = encode(&[Change::Memory(
                super::super::engine_id("a"),
                Some(Box::new(memory("a"))),
            )])
            .unwrap();
            tables.journal.commit(&batch).unwrap();
        }
        let tables = crate::db::engine::reopen_core(&dir);
        let core = tables.core.as_ref().unwrap();
        assert!(memories::row(core, "a").is_some());
        assert_eq!(
            core.search
                .search(&rusty_multimodal_db_engine::fulltext::Query::any_of([
                    "quokka"
                ]))
                .len(),
            1,
            "the replayed memory is searchable"
        );
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tables_without_the_core_refuse_a_journal_holding_core_changes() {
        let dir =
            std::env::temp_dir().join(format!("remind_me_engine_core_dark_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut tables = EngineTables::open_with_core(&dir).unwrap();
            let batch = encode(&[Change::Outbox(1, None)]).unwrap();
            tables.journal.commit(&batch).unwrap();
        }
        let refused = crate::db::engine::retry_while_locked(|| EngineTables::open(&dir));
        assert!(
            matches!(&refused, Err(StoreError::Engine(why)) if why.contains("cannot apply")),
            "{refused:?}"
        );
        drop(refused);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_without_the_core_are_refused() {
        let mut tables = EngineTables::open_temporary().unwrap();
        assert!(tables.commit(vec![Change::Outbox(1, None)]).is_err());
    }
}
