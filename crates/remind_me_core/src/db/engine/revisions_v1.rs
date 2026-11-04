//! The `memory_revisions` table as the engine stored it up to schema v32,
//! and its upgrade to the current layout.
//!
//! Schema v33 added `project` to [`RevisionRecord`] (card #702) so a revert
//! can restore it. The engine encodes a record positionally, so the table's
//! tag moved from `@1` to `@2`. As [`super::memories_v31`] does for
//! `memories`, this reads a `@1` table under the layout it was written with
//! and rewrites it under the current tag. Its revisions read the project as
//! unknown, and a revert to one leaves the memory's project alone.

use super::revisions::{ByMemory, EditedAt, RevisionRecord, RevisionTable};
use super::{engine_error, open_core};
use crate::db::Result;
use rusty_multimodal_db_engine::generic::insert_log::{self, LogEntry};
use rusty_multimodal_db_engine::generic::record_blob::{self, GenericRecordBlob};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// One `memory_revisions` row as schema v32 and earlier stored it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RevisionRecordV1 {
    pub(crate) id: i64,
    pub(crate) memory_id: String,
    pub(crate) content: String,
    pub(crate) category: String,
    pub(crate) tags: String,
    pub(crate) metadata: String,
    pub(crate) sensitive: Option<bool>,
    pub(crate) edited_at: String,
    pub(crate) edited_us: i64,
    pub(crate) revision_reason: Option<String>,
}

impl RevisionRecordV1 {
    /// The row under the current layout, its project unknown.
    fn into_current(self) -> RevisionRecord {
        RevisionRecord {
            id: self.id,
            memory_id: self.memory_id,
            content: self.content,
            category: self.category,
            tags: self.tags,
            metadata: self.metadata,
            sensitive: self.sensitive,
            edited_at: self.edited_at,
            edited_us: self.edited_us,
            revision_reason: self.revision_reason,
            project: None,
        }
    }
}

impl Record for RevisionRecordV1 {
    type Id = i64;
    fn id(&self) -> i64 {
        self.id
    }
}

impl SchemaTag for RevisionRecordV1 {
    /// The tag every table written before schema v33 carries.
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::RevisionRecord@1";
}

impl IndexedField<ByMemory> for RevisionRecordV1 {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.memory_id
    }
}

impl ScannableField<EditedAt> for RevisionRecordV1 {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.edited_us
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.edited_us = value;
    }
}

/// Open the `memory_revisions` table at `path`, creating it if absent, and
/// upgrading it first when it was written under the v32 layout.
///
/// # Errors
///
/// [`crate::db::StoreError::Engine`] if the table cannot be read under
/// either layout; the message carries both failures.
pub(crate) fn open_revisions(path: &Path) -> Result<RevisionTable> {
    if !path.exists() {
        return open_core(path);
    }
    match RevisionTable::open_portable(path) {
        Ok(table) => Ok(table),
        Err(current) => upgrade(path).map_err(|legacy| {
            engine_error(format!(
                "{current}; reading it under the pre-v33 layout instead: {legacy}"
            ))
        }),
    }
}

/// Upgrade the table at `path` to the current layout, or finish an upgrade
/// an earlier attempt left part-way.
///
/// Every step that writes happens only once the step before it is durable,
/// so a crash or error anywhere leaves a state this function can resume
/// from, and no legacy log entry loses its last durable copy:
///
/// 1. Read only: the base records, from the blob under either tag (`@2`
///    once an earlier attempt published it), and the `@1` insert log.
/// 2. Publish the folded records as a `@2` blob. The engine installs it by
///    write-to-temp, `fsync`, rename and directory `fsync`, so the old blob
///    stays whole until the new one is.
/// 3. Only then remove the `@1` log, whose every entry the new blob holds.
///    A retry between 2 and 3 folds the log into the `@2` blob again, which
///    changes nothing: an item overwrites itself, a tombstone finds nothing.
fn upgrade(path: &Path) -> Result<RevisionTable> {
    let records = fold(path)?;
    publish(path, &records)?;
    insert_log::clear(&insert_log::log_path(path)).map_err(engine_error)?;
    RevisionTable::open(records, path).map_err(engine_error)
}

/// The table's records under the current layout: its blob, `@2` or `@1`,
/// with its `@1` insert log applied. Reads only.
fn fold(path: &Path) -> Result<Vec<RevisionRecord>> {
    let blob = record_blob::blob_path(path);
    let base = match record_blob::read::<RevisionRecord>(&blob) {
        Ok(published) => published,
        Err(current) => record_blob::read::<RevisionRecordV1>(&blob)
            .map_err(|legacy| engine_error(format!("{current}; {legacy}")))?
            .into_iter()
            .map(RevisionRecordV1::into_current)
            .collect(),
    };
    let logged = insert_log::read_entries::<RevisionRecordV1, i64>(
        &insert_log::log_path(path),
        RevisionRecordV1::SCHEMA_TAG,
    )
    .map_err(engine_error)?;
    Ok(apply(base, logged))
}

/// `records` with each log entry applied in order, as the engine folds its
/// own log: an item replaces the record with its id in place or is
/// appended, and a tombstone removes the record it names, if any.
fn apply(
    records: Vec<RevisionRecord>,
    logged: Vec<LogEntry<RevisionRecordV1, i64>>,
) -> Vec<RevisionRecord> {
    let mut slots: Vec<Option<RevisionRecord>> = records.into_iter().map(Some).collect();
    let mut position: HashMap<i64, usize> = slots
        .iter()
        .flatten()
        .enumerate()
        .map(|(at, record)| (record.id, at))
        .collect();
    for entry in logged {
        match entry {
            LogEntry::Item(legacy) => {
                let record = legacy.into_current();
                match position.get(&record.id) {
                    Some(&at) => slots[at] = Some(record),
                    None => {
                        position.insert(record.id, slots.len());
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

/// Durably install `records` as the table's `@2` blob.
fn publish(path: &Path, records: &[RevisionRecord]) -> Result<()> {
    GenericRecordBlob::new(records)
        .encode()
        .and_then(|encoded| encoded.write(&record_blob::blob_path(path)))
        .map_err(engine_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_multimodal_db_engine::durability::record_blob::TEMP_SUFFIX;
    use rusty_multimodal_db_engine::generic::query::{AllIds, GetById};
    use rusty_multimodal_db_engine::generic::GenericMmapStore;

    type RevisionTableV1 = GenericMmapStore<RevisionRecordV1, ByMemory, EditedAt>;

    fn legacy(id: i64, content: &str) -> RevisionRecordV1 {
        RevisionRecordV1 {
            id,
            memory_id: "m1".to_string(),
            content: content.to_string(),
            category: "fact".to_string(),
            tags: "[]".to_string(),
            metadata: "{}".to_string(),
            sensitive: Some(false),
            edited_at: "2026-11-01T00:00:00+00:00".to_string(),
            edited_us: 1,
            revision_reason: Some("why".to_string()),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("remind_me_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_v1_table_is_read_under_its_own_layout_and_rewritten() {
        let dir = scratch("engine_revisions_v1");
        let path = dir.join("memory_revisions.mmap");
        {
            // One row in the blob, one in the insert log: both must survive.
            let mut table = RevisionTableV1::create(vec![legacy(1, "a")], &path).unwrap();
            table.insert(legacy(2, "b")).unwrap();
        }
        assert!(
            RevisionTable::open_portable(&path).is_err(),
            "the current type refuses the old layout by name"
        );

        let table = open_revisions(&path).unwrap();
        let mut ids = table.all_ids();
        ids.sort_unstable();
        assert_eq!(ids, [1, 2]);
        let a = table.get(1).unwrap();
        assert_eq!(a.content, "a");
        assert_eq!(a.revision_reason.as_deref(), Some("why"));
        assert_eq!(a.project, None, "unknown, not 'no project'");
        drop(table);

        let again = RevisionTable::open_portable(&path).unwrap();
        assert_eq!(again.all_ids().len(), 2, "rewritten under the current tag");
        drop(again);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A v1 table holding revision 2 in its blob, revision 3 only in its
    /// insert log, and a tombstone for revision 1, which the blob still has.
    fn legacy_table_with_log(path: &Path) {
        let mut table =
            RevisionTableV1::create(vec![legacy(1, "a"), legacy(2, "b")], path).unwrap();
        table.insert(legacy(3, "c")).unwrap();
        table.delete(1).unwrap();
    }

    fn sorted_ids(table: &RevisionTable) -> Vec<i64> {
        let mut ids = table.all_ids();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn a_failed_publish_leaves_the_legacy_table_whole_and_a_retry_upgrades_it() {
        let dir = scratch("engine_revisions_failed_publish");
        let path = dir.join("memory_revisions.mmap");
        legacy_table_with_log(&path);
        // A directory where the new blob's temp file goes makes publication fail.
        let mut temp = record_blob::blob_path(&path).into_os_string();
        temp.push(TEMP_SUFFIX);
        std::fs::create_dir(&temp).unwrap();

        assert!(open_revisions(&path).is_err(), "publication must fail");
        let kept = RevisionTableV1::read_portable_records(&path).unwrap();
        let mut ids: Vec<i64> = kept.iter().map(|r| r.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, [2, 3], "the v1 blob and log are both intact");

        std::fs::remove_dir(&temp).unwrap();
        let table = open_revisions(&path).unwrap();
        assert_eq!(
            sorted_ids(&table),
            [2, 3],
            "log-only row kept, tombstone honoured"
        );
        assert_eq!(table.get(3).unwrap().content, "c");
        drop(table);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_upgrade_interrupted_after_publication_resumes_from_the_mixed_layout() {
        let dir = scratch("engine_revisions_mixed");
        let path = dir.join("memory_revisions.mmap");
        legacy_table_with_log(&path);
        // A crash between publishing the v2 blob and removing the v1 log.
        publish(&path, &fold(&path).unwrap()).unwrap();
        assert!(
            insert_log::log_path(&path).exists(),
            "the v1 log is still there"
        );
        assert!(RevisionTable::open_portable(&path).is_err(), "mixed layout");

        let table = open_revisions(&path).unwrap();
        assert_eq!(
            sorted_ids(&table),
            [2, 3],
            "the replayed log changes nothing"
        );
        assert_eq!(table.get(3).unwrap().content, "c");
        drop(table);
        let again = RevisionTable::open_portable(&path).unwrap();
        assert_eq!(sorted_ids(&again), [2, 3], "finished: no upgrade needed");
        drop(again);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_torn_temp_blob_from_an_interrupted_publish_is_ignored() {
        let dir = scratch("engine_revisions_torn_temp");
        let path = dir.join("memory_revisions.mmap");
        legacy_table_with_log(&path);
        let mut temp = record_blob::blob_path(&path).into_os_string();
        temp.push(TEMP_SUFFIX);
        std::fs::write(&temp, b"half a blob").unwrap();

        let table = open_revisions(&path).unwrap();
        assert_eq!(sorted_ids(&table), [2, 3]);
        drop(table);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_table_under_neither_layout_names_both_failures() {
        let dir = scratch("engine_revisions_garbage");
        let path = dir.join("memory_revisions.mmap");
        std::fs::write(&path, b"not a table").unwrap();
        let refused = open_revisions(&path).map(|_| ()).err();
        assert!(
            matches!(&refused, Some(crate::db::StoreError::Engine(why)) if why.contains("pre-v33")),
            "{refused:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
