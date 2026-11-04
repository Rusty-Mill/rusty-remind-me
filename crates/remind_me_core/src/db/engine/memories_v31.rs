//! The `memories` table as the engine stored it up to schema v31, and its
//! upgrade to the current layout.
//!
//! The engine encodes a record positionally (bincode), so the thirteen
//! columns schema v32 added to [`MemoryRow`] changed the stored layout, and
//! the table's schema tag moved from `@1` to `@2`. A `@1` table is refused
//! by name when opened as the current type; this module reads it under the
//! layout it was written with, fills the new columns with the schema's
//! defaults, and rewrites it under the current tag. The slot file keeps
//! its ids and creation stamps, so nothing else about the table moves.
//!
//! The journal and undo log are JSON and need none of this: a row they
//! hold from before v32 reads through `#[serde(default)]` on
//! [`MemoryRow`].

use super::memories::{ByDoc, CreatedAt, MemoryRecord, MemoryRow, MemoryTable};
use super::{engine_error, legacy_upgrade, open_core};
use crate::db::Result;
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

/// One `memories` row as schema v31 stored it: the 28 columns of the time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MemoryRowV31 {
    pub(crate) id: String,
    pub(crate) content: String,
    pub(crate) category: String,
    pub(crate) tags: String,
    pub(crate) source: String,
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

impl MemoryRowV31 {
    /// The row under the current layout, every v32 column at its default.
    fn into_current(self) -> MemoryRow {
        MemoryRow {
            id: self.id,
            content: self.content,
            category: self.category,
            tags: self.tags,
            source: self.source,
            metadata: self.metadata,
            created_at: self.created_at,
            updated_at: self.updated_at,
            capture_id: self.capture_id,
            subject: self.subject,
            predicate: self.predicate,
            object: self.object,
            superseded_by: self.superseded_by,
            decay_rate: self.decay_rate,
            vitality: self.vitality,
            base_weight: self.base_weight,
            access_count: self.access_count,
            accessed_at: self.accessed_at,
            doc_id: self.doc_id,
            chunk_index: self.chunk_index,
            remind_at: self.remind_at,
            sensitive: self.sensitive,
            memory_type: self.memory_type,
            status: self.status,
            node_id: self.node_id,
            client: self.client,
            source_capture_id: self.source_capture_id,
            deleted_at: self.deleted_at,
            project: None,
            session_id: None,
            git_remote: None,
            git_branch: None,
            git_sha: None,
            cwd: None,
            valid_from: None,
            valid_until: None,
            confidence: crate::models::default_confidence(),
            verified_at: None,
            outcome: None,
            written_by: crate::models::default_written_by(),
            capture_method: crate::models::default_capture_method(),
        }
    }
}

/// A memory as the engine stored it up to v31: the same keys as
/// [`MemoryRecord`] around a [`MemoryRowV31`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MemoryRecordV31 {
    pub(crate) engine_id: Uuid,
    pub(crate) doc_key: String,
    pub(crate) created_us: i64,
    pub(crate) row: MemoryRowV31,
}

impl MemoryRecordV31 {
    /// The record under the current layout. Its engine id, derived from
    /// the row's id, is unchanged.
    fn into_current(self) -> MemoryRecord {
        MemoryRecord::new(self.row.into_current())
    }
}

impl Record for MemoryRecordV31 {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for MemoryRecordV31 {
    /// The tag every table written before schema v32 carries.
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::MemoryRecord@1";
}

impl IndexedField<ByDoc> for MemoryRecordV31 {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.doc_key
    }
}

impl ScannableField<CreatedAt> for MemoryRecordV31 {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.created_us
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.created_us = value;
    }
}

/// Open the `memories` table at `path`, creating it if absent, and
/// upgrading it first when it was written under the v31 layout.
///
/// # Errors
///
/// [`crate::db::StoreError::Engine`] if the table cannot be read under
/// either layout; the message carries both failures.
pub(crate) fn open_memories(path: &Path) -> Result<MemoryTable> {
    if !path.exists() {
        return open_core(path);
    }
    match MemoryTable::open_portable(path) {
        Ok(table) => Ok(table),
        Err(current) => upgrade(path).map_err(|legacy| {
            engine_error(format!(
                "{current}; reading it under the pre-v32 layout instead: {legacy}"
            ))
        }),
    }
}

/// Rewrite the v31 table at `path` under the current layout, crash-safely:
/// see [`legacy_upgrade`].
fn upgrade(path: &Path) -> Result<MemoryTable> {
    legacy_upgrade::upgrade(path, MemoryRecordV31::into_current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::engine::engine_id;
    use crate::db::engine::memories::MemoryRow;
    use crate::db::memories::NewMemory;
    use rusty_multimodal_db_engine::durability::record_blob::TEMP_SUFFIX;
    use rusty_multimodal_db_engine::generic::query::{AllIds, GetById};
    use rusty_multimodal_db_engine::generic::{insert_log, record_blob, GenericMmapStore};

    /// The table as a build before v32 opened it.
    type MemoryTableV31 = GenericMmapStore<MemoryRecordV31, ByDoc, CreatedAt>;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    /// A v31 record as a build before v32 would have written it.
    fn legacy(id: &str) -> MemoryRecordV31 {
        let new = NewMemory {
            doc_id: Some("doc".into()),
            tags: vec!["t".into()],
            sensitive: true,
            ..NewMemory::new(id, format!("content {id}"), NOW)
        };
        let now = MemoryRow::from_new(&new);
        MemoryRecordV31 {
            engine_id: engine_id(id),
            doc_key: "doc".to_string(),
            created_us: crate::db::engine::micros(NOW),
            row: MemoryRowV31 {
                id: now.id,
                content: now.content,
                category: now.category,
                tags: now.tags,
                source: now.source,
                metadata: now.metadata,
                created_at: now.created_at,
                updated_at: now.updated_at,
                capture_id: now.capture_id,
                subject: now.subject,
                predicate: now.predicate,
                object: now.object,
                superseded_by: now.superseded_by,
                decay_rate: now.decay_rate,
                vitality: now.vitality,
                base_weight: now.base_weight,
                access_count: now.access_count,
                accessed_at: now.accessed_at,
                doc_id: now.doc_id,
                chunk_index: now.chunk_index,
                remind_at: now.remind_at,
                sensitive: now.sensitive,
                memory_type: now.memory_type,
                status: now.status,
                node_id: now.node_id,
                client: now.client,
                source_capture_id: now.source_capture_id,
                deleted_at: now.deleted_at,
            },
        }
    }

    #[test]
    fn a_v31_table_is_read_under_its_own_layout_and_rewritten() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_memories_v31_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memories.mmap");
        {
            // One row in the blob, one in the insert log: both must survive.
            let mut table = MemoryTableV31::create(vec![legacy("a")], &path).unwrap();
            table.insert(legacy("b")).unwrap();
        }
        assert!(
            MemoryTable::open_portable(&path).is_err(),
            "the current type refuses the old layout by name"
        );

        let table = open_memories(&path).unwrap();
        let mut ids: Vec<String> = table
            .all_ids()
            .into_iter()
            .filter_map(|id| table.get(id))
            .map(|r| r.row.id)
            .collect();
        ids.sort();
        assert_eq!(ids, ["a", "b"]);
        let a = table.get(engine_id("a")).unwrap().row;
        assert_eq!(a.content, "content a");
        assert!(a.sensitive);
        assert_eq!(a.doc_id.as_deref(), Some("doc"));
        assert_eq!(a.confidence, 1.0);
        assert_eq!(a.written_by, "unknown");
        assert_eq!(a.capture_method, "manual");
        assert_eq!(a.project, None);
        drop(table);

        // Rewritten: the next open needs no upgrade, and reads the same.
        let again = MemoryTable::open_portable(&path).unwrap();
        assert_eq!(again.all_ids().len(), 2);
        drop(again);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("remind_me_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A v31 table whose insert log holds a new row "c", a newer version
    /// of blob row "b", and a tombstone for blob row "a".
    fn legacy_table_with_log(path: &Path) {
        let mut table = MemoryTableV31::create(vec![legacy("a"), legacy("b")], path).unwrap();
        table.insert(legacy("c")).unwrap();
        let mut newer = legacy("b");
        newer.row.content = "content b, edited".to_string();
        table.replace(newer).unwrap();
        table.delete(engine_id("a")).unwrap();
    }

    /// The table's string ids, sorted, and row "b"'s content.
    fn ids_and_b(table: &MemoryTable) -> (Vec<String>, String) {
        let mut ids: Vec<String> = table
            .all_ids()
            .into_iter()
            .filter_map(|id| table.get(id))
            .map(|r| r.row.id)
            .collect();
        ids.sort();
        (ids, table.get(engine_id("b")).unwrap().row.content)
    }

    fn temp_blob(path: &Path) -> std::ffi::OsString {
        let mut temp = record_blob::blob_path(path).into_os_string();
        temp.push(TEMP_SUFFIX);
        temp
    }

    #[test]
    fn a_failed_publish_leaves_the_v31_table_whole_and_a_retry_upgrades_it() {
        let dir = scratch("engine_memories_v31_failed_publish");
        let path = dir.join("memories.mmap");
        legacy_table_with_log(&path);
        // A directory where the new blob's temp file goes makes publication fail.
        let temp = temp_blob(&path);
        std::fs::create_dir(&temp).unwrap();

        assert!(open_memories(&path).is_err(), "publication must fail");
        let kept = MemoryTableV31::read_portable_records(&path).unwrap();
        let mut ids: Vec<&str> = kept.iter().map(|r| r.row.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["b", "c"], "the v31 blob and log are both intact");

        std::fs::remove_dir(&temp).unwrap();
        let table = open_memories(&path).unwrap();
        assert_eq!(
            ids_and_b(&table),
            (vec!["b".into(), "c".into()], "content b, edited".into()),
            "log-only row and newer version kept, tombstone honoured"
        );
        drop(table);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_upgrade_interrupted_after_publication_resumes_from_the_mixed_layout() {
        let dir = scratch("engine_memories_v31_mixed");
        let path = dir.join("memories.mmap");
        legacy_table_with_log(&path);
        // A crash between publishing the v32 blob and removing the v31 log.
        let folded = legacy_upgrade::fold(&path, MemoryRecordV31::into_current).unwrap();
        legacy_upgrade::publish(&path, &folded).unwrap();
        assert!(
            insert_log::log_path(&path).exists(),
            "the v31 log is still there"
        );
        assert!(MemoryTable::open_portable(&path).is_err(), "mixed layout");

        let table = open_memories(&path).unwrap();
        assert_eq!(
            ids_and_b(&table),
            (vec!["b".into(), "c".into()], "content b, edited".into()),
            "the replayed log changes nothing"
        );
        drop(table);
        let again = MemoryTable::open_portable(&path).unwrap();
        assert_eq!(again.all_ids().len(), 2, "finished: no upgrade needed");
        drop(again);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_torn_temp_blob_from_an_interrupted_publish_is_ignored() {
        let dir = scratch("engine_memories_v31_torn_temp");
        let path = dir.join("memories.mmap");
        legacy_table_with_log(&path);
        std::fs::write(temp_blob(&path), b"half a blob").unwrap();

        let table = open_memories(&path).unwrap();
        assert_eq!(ids_and_b(&table).0, ["b", "c"]);
        drop(table);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_row_deleted_then_logged_again_survives_the_upgrade() {
        let dir = scratch("engine_memories_v31_reinsert");
        let path = dir.join("memories.mmap");
        {
            let mut table = MemoryTableV31::create(vec![legacy("a")], &path).unwrap();
            table.delete(engine_id("a")).unwrap();
            table.insert(legacy("a")).unwrap();
        }
        let table = open_memories(&path).unwrap();
        assert!(table.get(engine_id("a")).is_some());
        drop(table);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_table_under_neither_layout_names_both_failures() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_memories_garbage_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memories.mmap");
        std::fs::write(&path, b"not a table").unwrap();
        let refused = open_memories(&path).map(|_| ()).err();
        assert!(
            matches!(&refused, Some(crate::db::StoreError::Engine(why)) if why.contains("pre-v32")),
            "{refused:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
