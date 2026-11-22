//! Storage for raw-transcript retention: `import_archives` (one row per
//! archived import) and `import_archive_spans` (which bytes of it produced
//! each memory), behind the [`ArchiveStore`] port (ADR-0035), which the
//! engine implements in `db::engine::archives`.
//!
//! Every read and write [`crate::archive`] makes goes through here. The
//! rules stay there: whether retention is on, where blobs go, span
//! clamping, the sensitive-memory gate, and pruning by age and size.

use super::backend::BackendLock;
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

/// The port for the retention tables (ADR-0035). [`conformance`] holds the
/// behaviour every implementation must show.
pub trait ArchiveStore {
    /// Record `row`, replacing any archive already recorded for its import.
    fn record(&mut self, row: &ArchiveRow) -> Result<()>;

    /// Record that bytes `byte_start..byte_end` of `import_id` produced
    /// `memory_id`, replacing any span recorded for it.
    fn record_span(
        &mut self,
        memory_id: &str,
        import_id: &str,
        byte_start: i64,
        byte_end: i64,
    ) -> Result<()>;

    /// Where `memory_id`'s bytes are: `None` if it has no span, or its
    /// span's import has no recorded archive.
    fn span_source(&self, memory_id: &str) -> Result<Option<SpanSource>>;

    /// The archive path and content hash recorded for `import_id`.
    fn blob_of(&self, import_id: &str) -> Result<Option<(String, String)>>;

    /// Remove `import_id`'s archive row and spans. A missing import is not
    /// an error.
    fn remove(&mut self, import_id: &str) -> Result<()>;

    /// How many recorded archives share the blob `hash`.
    fn count_with_hash(&self, hash: &str) -> Result<usize>;

    /// Every recorded archive, oldest `archived_at` first (compared as
    /// text).
    fn oldest_first(&self) -> Result<Vec<ArchiveRow>>;

    /// How many spans are recorded: for `import_id`, or in all when `None`.
    fn span_count(&self, import_id: Option<&str>) -> Result<usize>;
}

/// The retention tables, on the engine.
pub struct Archives<'c> {
    backend: &'c BackendLock,
}

impl<'c> Archives<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            backend: store.backend(),
        }
    }

    /// See [`ArchiveStore::record`].
    pub fn record(&self, row: &ArchiveRow) -> Result<()> {
        ArchiveStore::record(&mut *self.backend.lock()?, row)
    }

    /// See [`ArchiveStore::record_span`].
    pub fn record_span(
        &self,
        memory_id: &str,
        import_id: &str,
        byte_start: i64,
        byte_end: i64,
    ) -> Result<()> {
        ArchiveStore::record_span(
            &mut *self.backend.lock()?,
            memory_id,
            import_id,
            byte_start,
            byte_end,
        )
    }

    /// See [`ArchiveStore::span_source`].
    pub fn span_source(&self, memory_id: &str) -> Result<Option<SpanSource>> {
        ArchiveStore::span_source(&*self.backend.lock()?, memory_id)
    }

    /// See [`ArchiveStore::blob_of`].
    pub fn blob_of(&self, import_id: &str) -> Result<Option<(String, String)>> {
        ArchiveStore::blob_of(&*self.backend.lock()?, import_id)
    }

    /// See [`ArchiveStore::remove`].
    pub fn remove(&self, import_id: &str) -> Result<()> {
        ArchiveStore::remove(&mut *self.backend.lock()?, import_id)
    }

    /// See [`ArchiveStore::count_with_hash`].
    pub fn count_with_hash(&self, hash: &str) -> Result<usize> {
        ArchiveStore::count_with_hash(&*self.backend.lock()?, hash)
    }

    /// See [`ArchiveStore::oldest_first`].
    pub fn oldest_first(&self) -> Result<Vec<ArchiveRow>> {
        ArchiveStore::oldest_first(&*self.backend.lock()?)
    }

    /// See [`ArchiveStore::span_count`].
    pub fn span_count(&self, import_id: Option<&str>) -> Result<usize> {
        ArchiveStore::span_count(&*self.backend.lock()?, import_id)
    }
}

/// The behaviour every [`ArchiveStore`] must show, run against each backend
/// by [`archive_conformance`] (ADR-0035).
#[doc(hidden)]
pub mod conformance {
    use super::{ArchiveRow, ArchiveStore};

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

    pub fn remove_takes_the_spans_and_leaves_a_shared_blob_counted(store: &mut dyn ArchiveStore) {
        store.record(&row("a", "h", "2026-09-02")).unwrap();
        store.record(&row("b", "h", "2026-09-01")).unwrap();
        store.record_span("m1", "a", 0, 5).unwrap();

        assert_eq!(store.span_source("m1").unwrap().unwrap().import_id, "a");
        assert_eq!(store.span_count(Some("a")).unwrap(), 1);
        store.remove("a").unwrap();
        assert!(store.span_source("m1").unwrap().is_none());
        assert_eq!(store.span_count(None).unwrap(), 0);
        assert_eq!(store.count_with_hash("h").unwrap(), 1);
        let ids: Vec<String> = store
            .oldest_first()
            .unwrap()
            .into_iter()
            .map(|r| r.import_id)
            .collect();
        assert_eq!(ids, vec!["b"]);
        // Removing again is not an error, as a delete of no rows is not.
        store.remove("a").unwrap();
    }

    pub fn recording_again_replaces_the_archive_and_the_span(store: &mut dyn ArchiveStore) {
        store.record(&row("a", "h1", "2026-09-01")).unwrap();
        store.record(&row("a", "h2", "2026-09-03")).unwrap();
        store.record_span("m1", "a", 0, 5).unwrap();
        store.record_span("m1", "a", 2, 9).unwrap();

        assert_eq!(
            store.oldest_first().unwrap(),
            vec![row("a", "h2", "2026-09-03")]
        );
        assert_eq!(store.count_with_hash("h1").unwrap(), 0);
        let span = store.span_source("m1").unwrap().unwrap();
        assert_eq!((span.byte_start, span.byte_end), (2, 9));
        assert_eq!(store.span_count(None).unwrap(), 1);
    }

    pub fn a_span_whose_import_has_no_archive_has_no_source(store: &mut dyn ArchiveStore) {
        store.record_span("m1", "gone", 0, 5).unwrap();
        assert!(store.span_source("m1").unwrap().is_none());
        assert!(store.blob_of("gone").unwrap().is_none());
        assert_eq!(store.span_count(Some("gone")).unwrap(), 1);
    }

    pub fn blobs_and_order_follow_the_recorded_archives(store: &mut dyn ArchiveStore) {
        store.record(&row("late", "h1", "2026-09-03")).unwrap();
        store.record(&row("early", "h2", "2026-09-01")).unwrap();
        assert_eq!(
            store.blob_of("late").unwrap(),
            Some(("/archive/h1".to_string(), "h1".to_string()))
        );
        let ids: Vec<String> = store
            .oldest_first()
            .unwrap()
            .into_iter()
            .map(|r| r.import_id)
            .collect();
        assert_eq!(ids, ["early", "late"]);
        assert_eq!(store.count_with_hash("missing").unwrap(), 0);
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`.
#[doc(hidden)]
#[macro_export]
macro_rules! archive_conformance {
    ($make:expr) => {
        use $crate::db::archives::conformance;

        #[test]
        fn remove_takes_the_spans_and_leaves_a_shared_blob_counted() {
            conformance::remove_takes_the_spans_and_leaves_a_shared_blob_counted(&mut $make);
        }
        #[test]
        fn recording_again_replaces_the_archive_and_the_span() {
            conformance::recording_again_replaces_the_archive_and_the_span(&mut $make);
        }
        #[test]
        fn a_span_whose_import_has_no_archive_has_no_source() {
            conformance::a_span_whose_import_has_no_archive_has_no_source(&mut $make);
        }
        #[test]
        fn blobs_and_order_follow_the_recorded_archives() {
            conformance::blobs_and_order_follow_the_recorded_archives(&mut $make);
        }
    };
}
#[doc(hidden)]
pub use crate::archive_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    /// The facade locks per call and reaches the engine through the port.
    #[test]
    fn the_facade_reads_back_what_it_records() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let archives = Archives::new(&store);
        let row = ArchiveRow {
            import_id: "a".to_string(),
            hash: "h".to_string(),
            filename: "chat.json".to_string(),
            archive_path: "/archive/h".to_string(),
            byte_len: 10,
            archived_at: "2026-09-01".to_string(),
        };
        archives.record(&row).unwrap();
        archives.record_span("m1", "a", 0, 5).unwrap();
        assert_eq!(archives.oldest_first().unwrap(), vec![row]);
        assert_eq!(archives.span_source("m1").unwrap().unwrap().byte_end, 5);
        assert_eq!(archives.blob_of("a").unwrap().unwrap().1, "h");
        assert_eq!(archives.count_with_hash("h").unwrap(), 1);
        assert_eq!(archives.span_count(Some("a")).unwrap(), 1);
        archives.remove("a").unwrap();
        assert_eq!(archives.span_count(None).unwrap(), 0);
    }
}
