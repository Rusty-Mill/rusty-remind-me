//! Storage for raw-transcript retention: `import_archives` (one row per
//! archived import) and `import_archive_spans` (which bytes of it produced
//! each memory), on the engine (`db::engine::archives`).
//!
//! Every read and write [`crate::archive`] makes goes through here. The
//! rules stay there: whether retention is on, where blobs go, span
//! clamping, the sensitive-memory gate, and pruning by age and size.

use super::engine::{self, EngineLock};
use super::{Result, Store};

/// One archived import.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveRow {
    pub import_id: String,
    pub hash: String,
    pub filename: String,
    pub archive_path: String,
    pub byte_len: i64,
    pub archived_at: String,
}

/// Where one memory's bytes are: its import, the span, and the archive.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanSource {
    pub import_id: String,
    pub byte_start: i64,
    pub byte_end: i64,
    pub archive_path: String,
    pub filename: String,
}

/// The retention tables, on the engine.
pub struct Archives<'c> {
    engine: &'c EngineLock,
}

impl<'c> Archives<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            engine: store.engine(),
        }
    }

    /// Record `row`, replacing any archive already recorded for its import.
    pub fn record(&self, row: &ArchiveRow) -> Result<()> {
        engine::archives::record(&mut self.engine.lock(), row)
    }

    /// Record that bytes `byte_start..byte_end` of `import_id` produced
    /// `memory_id`, replacing any span recorded for it.
    pub fn record_span(
        &self,
        memory_id: &str,
        import_id: &str,
        byte_start: i64,
        byte_end: i64,
    ) -> Result<()> {
        engine::archives::record_span(
            &mut self.engine.lock(),
            memory_id,
            import_id,
            byte_start,
            byte_end,
        )
    }

    /// Where `memory_id`'s bytes are, if it has a span in a recorded archive.
    pub fn span_source(&self, memory_id: &str) -> Result<Option<SpanSource>> {
        Ok(engine::archives::span_source(
            &self.engine.lock(),
            memory_id,
        ))
    }

    /// The archive path and content hash recorded for `import_id`.
    pub fn blob_of(&self, import_id: &str) -> Result<Option<(String, String)>> {
        Ok(engine::archives::blob_of(&self.engine.lock(), import_id))
    }

    /// Remove `import_id`'s archive row and spans. The blob is not touched.
    pub fn remove(&self, import_id: &str) -> Result<()> {
        engine::archives::remove(&mut self.engine.lock(), import_id)
    }

    /// How many recorded archives share the blob `hash`.
    pub fn count_with_hash(&self, hash: &str) -> Result<usize> {
        Ok(engine::archives::count_with_hash(&self.engine.lock(), hash))
    }

    /// Every recorded archive, oldest `archived_at` first.
    pub fn oldest_first(&self) -> Result<Vec<ArchiveRow>> {
        Ok(engine::archives::oldest_first(&self.engine.lock()))
    }

    /// How many spans are recorded: for `import_id`, or in all when `None`.
    pub fn span_count(&self, import_id: Option<&str>) -> Result<usize> {
        Ok(engine::archives::span_count(&self.engine.lock(), import_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn row(import_id: &str, hash: &str, archived_at: &str) -> ArchiveRow {
        ArchiveRow {
            import_id: import_id.to_string(),
            hash: hash.to_string(),
            filename: "chat.json".to_string(),
            archive_path: format!("/archive/{hash}"),
            byte_len: 10,
            archived_at: archived_at.to_string(),
        }
    }

    #[test]
    fn remove_takes_the_spans_and_leaves_a_shared_blob_counted() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let archives = Archives::new(&store);
        archives.record(&row("a", "h", "2026-09-02")).unwrap();
        archives.record(&row("b", "h", "2026-09-01")).unwrap();
        archives.record_span("m1", "a", 0, 5).unwrap();

        assert_eq!(archives.span_source("m1").unwrap().unwrap().import_id, "a");
        assert_eq!(archives.span_count(Some("a")).unwrap(), 1);
        archives.remove("a").unwrap();
        assert!(archives.span_source("m1").unwrap().is_none());
        assert_eq!(archives.span_count(None).unwrap(), 0);
        assert_eq!(archives.count_with_hash("h").unwrap(), 1);
        let ids: Vec<String> = archives
            .oldest_first()
            .unwrap()
            .into_iter()
            .map(|r| r.import_id)
            .collect();
        assert_eq!(ids, vec!["b"]);
        // Removing again is not an error, as a delete of no rows is not.
        archives.remove("a").unwrap();
    }

    #[test]
    fn recording_again_replaces_the_archive_and_the_span() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let archives = Archives::new(&store);
        archives.record(&row("a", "h1", "2026-09-01")).unwrap();
        archives.record(&row("a", "h2", "2026-09-03")).unwrap();
        archives.record_span("m1", "a", 0, 5).unwrap();
        archives.record_span("m1", "a", 2, 9).unwrap();

        assert_eq!(
            archives.oldest_first().unwrap(),
            vec![row("a", "h2", "2026-09-03")]
        );
        assert_eq!(archives.count_with_hash("h1").unwrap(), 0);
        let span = archives.span_source("m1").unwrap().unwrap();
        assert_eq!((span.byte_start, span.byte_end), (2, 9));
        assert_eq!(archives.span_count(None).unwrap(), 1);
    }

    #[test]
    fn a_span_whose_import_has_no_archive_has_no_source() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let archives = Archives::new(&store);
        archives.record_span("m1", "gone", 0, 5).unwrap();
        assert!(archives.span_source("m1").unwrap().is_none());
        assert!(archives.blob_of("gone").unwrap().is_none());
        assert_eq!(archives.span_count(Some("gone")).unwrap(), 1);
    }
}
