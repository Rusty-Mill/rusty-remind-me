//! Import archives and their spans on the engine (ADR-0023, phase 4c): the
//! engine's [`ArchiveStore`] adapter (ADR-0035). The behaviour matches the
//! old SQLite statements one for one.

use super::{deleted, engine_error, engine_id, ensure_same_id, micros, EngineTables};
use crate::db::archives::{ArchiveRow, ArchiveStore, SpanSource};
use crate::db::Result;
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Index marker: an archive by the hash of its blob, which several imports
/// can share.
pub struct ByHash;
/// Index marker: a span by the import that produced it.
pub struct ByImport;
/// Slot marker: when an import was archived, in µs since the epoch.
pub struct ArchivedAt;
/// Slot marker: where a span starts.
pub struct ByteStart;

pub(crate) type ArchiveTable = GenericMmapStore<ArchiveRecord, ByHash, ArchivedAt>;
pub(crate) type SpanTable = GenericMmapStore<SpanRecord, ByImport, ByteStart>;

/// One `import_archives` row, keyed by its import id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveRecord {
    engine_id: Uuid,
    import_id: String,
    hash: String,
    filename: String,
    archive_path: String,
    byte_len: i64,
    archived_at: String,
    archived_us: i64,
}

impl ArchiveRecord {
    fn new(row: &ArchiveRow) -> Self {
        Self {
            engine_id: engine_id(&row.import_id),
            import_id: row.import_id.clone(),
            hash: row.hash.clone(),
            filename: row.filename.clone(),
            archive_path: row.archive_path.clone(),
            byte_len: row.byte_len,
            archived_at: row.archived_at.clone(),
            archived_us: micros(&row.archived_at),
        }
    }

    fn to_row(&self) -> ArchiveRow {
        ArchiveRow {
            import_id: self.import_id.clone(),
            hash: self.hash.clone(),
            filename: self.filename.clone(),
            archive_path: self.archive_path.clone(),
            byte_len: self.byte_len,
            archived_at: self.archived_at.clone(),
        }
    }
}

impl Record for ArchiveRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for ArchiveRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::ArchiveRecord@1";
}

impl IndexedField<ByHash> for ArchiveRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.hash
    }
}

impl ScannableField<ArchivedAt> for ArchiveRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.archived_us
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.archived_us = value;
    }
}

/// One `import_archive_spans` row, keyed by the memory it produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpanRecord {
    engine_id: Uuid,
    memory_id: String,
    import_id: String,
    byte_start: i64,
    byte_end: i64,
}

impl Record for SpanRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for SpanRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::SpanRecord@1";
}

impl IndexedField<ByImport> for SpanRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.import_id
    }
}

impl ScannableField<ByteStart> for SpanRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.byte_start
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.byte_start = value;
    }
}

impl ArchiveStore for EngineTables {
    /// Replaces any archive recorded for the import, as `INSERT OR REPLACE`
    /// does.
    fn record(&mut self, row: &ArchiveRow) -> Result<()> {
        let record = ArchiveRecord::new(row);
        match self.archives.get(record.engine_id) {
            Some(stored) => {
                ensure_same_id(&stored.import_id, &row.import_id)?;
                self.archives.replace(record).map_err(engine_error)
            }
            None => self.archives.insert(record).map_err(engine_error),
        }
    }

    fn record_span(
        &mut self,
        memory_id: &str,
        import_id: &str,
        byte_start: i64,
        byte_end: i64,
    ) -> Result<()> {
        let record = SpanRecord {
            engine_id: engine_id(memory_id),
            memory_id: memory_id.to_string(),
            import_id: import_id.to_string(),
            byte_start,
            byte_end,
        };
        match self.spans.get(record.engine_id) {
            Some(stored) => {
                ensure_same_id(&stored.memory_id, memory_id)?;
                self.spans.replace(record).map_err(engine_error)
            }
            None => self.spans.insert(record).map_err(engine_error),
        }
    }

    /// An inner join, as the SQL is: a span whose import has no archive
    /// has no source.
    fn span_source(&self, memory_id: &str) -> Result<Option<SpanSource>> {
        let Some(span) = self
            .spans
            .get(engine_id(memory_id))
            .filter(|span| span.memory_id == memory_id)
        else {
            return Ok(None);
        };
        Ok(archive(self, &span.import_id).map(|archive| SpanSource {
            import_id: span.import_id,
            byte_start: span.byte_start,
            byte_end: span.byte_end,
            archive_path: archive.archive_path,
            filename: archive.filename,
        }))
    }

    fn blob_of(&self, import_id: &str) -> Result<Option<(String, String)>> {
        Ok(archive(self, import_id).map(|a| (a.archive_path, a.hash)))
    }

    fn remove(&mut self, import_id: &str) -> Result<()> {
        for span in spans_of(self, import_id) {
            deleted(self.spans.delete(span))?;
        }
        if archive(self, import_id).is_some() {
            deleted(self.archives.delete(engine_id(import_id)))?;
        }
        Ok(())
    }

    fn count_with_hash(&self, hash: &str) -> Result<usize> {
        Ok(FilterEq::<ArchiveRecord, ByHash>::filter_eq(&self.archives, &hash.to_string()).len())
    }

    /// Archives recorded at the same instant come in import-id order, where
    /// SQLite leaves them unspecified.
    fn oldest_first(&self) -> Result<Vec<ArchiveRow>> {
        let mut records: Vec<ArchiveRecord> = self
            .archives
            .all_ids()
            .into_iter()
            .filter_map(|id| self.archives.get(id))
            .collect();
        records.sort_by(|a, b| {
            (a.archived_at.as_str(), a.import_id.as_str())
                .cmp(&(b.archived_at.as_str(), b.import_id.as_str()))
        });
        Ok(records.iter().map(ArchiveRecord::to_row).collect())
    }

    fn span_count(&self, import_id: Option<&str>) -> Result<usize> {
        Ok(match import_id {
            Some(import_id) => spans_of(self, import_id).len(),
            None => self.spans.all_ids().len(),
        })
    }
}

fn archive(tables: &EngineTables, import_id: &str) -> Option<ArchiveRecord> {
    tables
        .archives
        .get(engine_id(import_id))
        .filter(|a| a.import_id == import_id)
}

fn spans_of(tables: &EngineTables, import_id: &str) -> Vec<Uuid> {
    FilterEq::<SpanRecord, ByImport>::filter_eq(&tables.spans, &import_id.to_string())
}

#[cfg(test)]
mod conformance_tests {
    use super::EngineTables;
    crate::db::archives::archive_conformance!(EngineTables::open_temporary().unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(import_id: &str, archived_at: &str) -> ArchiveRow {
        ArchiveRow {
            import_id: import_id.to_string(),
            hash: "h".to_string(),
            filename: "chat.json".to_string(),
            archive_path: "/archive/h".to_string(),
            byte_len: 10,
            archived_at: archived_at.to_string(),
        }
    }

    #[test]
    fn records_survive_a_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_archives_reopen_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            tables
                .record(&row("a", "2026-09-01T00:00:00+00:00"))
                .unwrap();
            tables.record_span("m1", "a", 0, 5).unwrap();
        }
        let tables = crate::db::engine::reopen(&dir);
        assert_eq!(
            tables.oldest_first().unwrap(),
            vec![row("a", "2026-09-01T00:00:00+00:00")]
        );
        assert_eq!(tables.span_source("m1").unwrap().unwrap().byte_end, 5);
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ties_in_archived_at_come_in_import_id_order() {
        let mut tables = EngineTables::open_temporary().unwrap();
        for id in ["c", "a", "b"] {
            tables.record(&row(id, "2026-09-01")).unwrap();
        }
        let ids: Vec<String> = tables
            .oldest_first()
            .unwrap()
            .into_iter()
            .map(|r| r.import_id)
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }
}
