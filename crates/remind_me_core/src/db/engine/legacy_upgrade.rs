//! Crash-safe rewrite of an engine table from a legacy record layout.
//!
//! A table whose record type gained fields moves to a new schema tag, and
//! an older table must be read under the layout it was written with and
//! rewritten under the current one. Both its blob and its insert log carry
//! the legacy tag, so the log has to go before the current type can open
//! the table. Removing it first loses every entry the blob does not hold
//! yet if the rewrite then fails. So [`upgrade`] runs in three steps, each
//! write waiting until the step before it is durable:
//!
//! 1. [`fold`], read only: the base records, from the blob under the
//!    current tag (once an earlier attempt published it) or the legacy one,
//!    with the legacy insert log applied.
//! 2. [`publish`]: the folded records become the blob under the current
//!    tag. The engine installs it by write-to-temp, `fsync`, rename and
//!    directory `fsync`, so the legacy blob stays whole until it is replaced.
//! 3. Only then is the legacy log removed: the new blob holds its entries.
//!
//! A retry resumes from any point. Between steps 2 and 3 it folds the log
//! into the published blob again, which changes nothing: an item
//! overwrites itself, and a tombstone finds nothing to remove.

use super::engine_error;
use crate::db::Result;
use rusty_multimodal_db_engine::generic::insert_log::{self, LogEntry};
use rusty_multimodal_db_engine::generic::mmap_field::MmapFieldValue;
use rusty_multimodal_db_engine::generic::record_blob::{self, GenericRecordBlob};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::hash::Hash;
use std::path::Path;

/// Rewrite the table at `path` from layout `L` to layout `R`, or finish a
/// rewrite an earlier attempt left part-way, and open it. `convert` maps a
/// legacy record to the current layout and keeps its id.
///
/// # Errors
///
/// [`crate::db::StoreError::Engine`] if the table reads under neither
/// layout, or a step fails. Nothing is written before every read succeeds,
/// and the legacy log stays until the new blob is durable.
pub(crate) fn upgrade<L, R, Index, Slot>(
    path: &Path,
    convert: fn(L) -> R,
) -> Result<GenericMmapStore<R, Index, Slot>>
where
    L: DeserializeOwned + SchemaTag,
    R: Record
        + IndexedField<Index>
        + ScannableField<Slot>
        + Clone
        + Serialize
        + DeserializeOwned
        + SchemaTag,
    R::Id: MmapFieldValue + Serialize + DeserializeOwned + Hash + Eq,
    R::ScanValue: MmapFieldValue,
{
    let records = fold(path, convert)?;
    publish(path, &records)?;
    insert_log::clear(&insert_log::log_path(path)).map_err(engine_error)?;
    GenericMmapStore::open(records, path).map_err(engine_error)
}

/// The table's records under layout `R`: its blob, under `R`'s tag or
/// `L`'s, with its `L` insert log applied. Reads only.
pub(crate) fn fold<L, R>(path: &Path, convert: fn(L) -> R) -> Result<Vec<R>>
where
    L: DeserializeOwned + SchemaTag,
    R: Record + DeserializeOwned + SchemaTag,
    R::Id: DeserializeOwned + Hash + Eq,
{
    let blob = record_blob::blob_path(path);
    let base = match record_blob::read::<R>(&blob) {
        Ok(published) => published,
        Err(current) => record_blob::read::<L>(&blob)
            .map_err(|legacy| engine_error(format!("{current}; {legacy}")))?
            .into_iter()
            .map(convert)
            .collect(),
    };
    let logged = insert_log::read_entries::<L, R::Id>(&insert_log::log_path(path), L::SCHEMA_TAG)
        .map_err(engine_error)?;
    Ok(apply(base, logged, convert))
}

/// `records` with each log entry applied in order, as the engine folds its
/// own log: an item replaces the record with its id in place or is
/// appended, and a tombstone removes the record it names, if any.
fn apply<L, R>(records: Vec<R>, logged: Vec<LogEntry<L, R::Id>>, convert: fn(L) -> R) -> Vec<R>
where
    R: Record,
    R::Id: Hash + Eq,
{
    let mut slots: Vec<Option<R>> = records.into_iter().map(Some).collect();
    let mut position: HashMap<R::Id, usize> = slots
        .iter()
        .flatten()
        .enumerate()
        .map(|(at, record)| (record.id(), at))
        .collect();
    for entry in logged {
        match entry {
            LogEntry::Item(legacy) => {
                let record = convert(legacy);
                match position.get(&record.id()) {
                    Some(&at) => slots[at] = Some(record),
                    None => {
                        position.insert(record.id(), slots.len());
                        slots.push(Some(record));
                    }
                }
            }
            LogEntry::Tombstone(id) => {
                if let Some(at) = position.remove(&id) {
                    slots[at] = None;
                }
            }
        }
    }
    slots.into_iter().flatten().collect()
}

/// Durably install `records` as the table's blob under `R`'s tag.
pub(crate) fn publish<R>(path: &Path, records: &[R]) -> Result<()>
where
    R: Serialize + SchemaTag,
{
    GenericRecordBlob::new(records)
        .encode()
        .and_then(|encoded| encoded.write(&record_blob::blob_path(path)))
        .map_err(engine_error)
}
