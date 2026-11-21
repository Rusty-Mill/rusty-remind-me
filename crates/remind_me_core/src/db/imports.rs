//! Storage for import bookkeeping: `chat_imports` (one row per imported
//! file), `dbs_imports` (one row per daily-backup-system item) and
//! `mempalace_imports` (one row per drawer), plus the reads that find what
//! an import wrote so it can be undone, behind the [`ImportStore`] port
//! (ADR-0035), which the engine's memories core implements in
//! `db::engine::imports`.
//!
//! Every read and write against these tables from [`crate::importer`],
//! [`crate::dbs_import`], [`crate::mempalace_import`] and
//! [`crate::undo_import`] goes through here. Reading the foreign SQLite
//! files those importers take in is not storage, so it stays with them
//! (through `db::legacy_sqlite`).
//!
//! The rules stay with the modules: when content counts as already
//! imported, when a changed dbs item supersedes its memory, and that a chat
//! import loses its tracking row only once nothing of it is left.

use super::backend::BackendLock;
use super::{Result, Store};
use std::collections::HashMap;

/// What a previous import recorded for one dbs item.
#[derive(Debug, Clone, PartialEq)]
pub struct DbsTracked {
    pub memory_id: String,
    pub content_hash: String,
}

/// A dbs item's key: its source and its id there.
pub type DbsKey = (String, String);

/// The port for the import bookkeeping tables and the reads that find what
/// an import wrote (ADR-0035). [`conformance`] holds the behaviour every
/// implementation must show.
pub trait ImportStore {
    /// Record the chat import `import_id` of `filename`, with its content
    /// hash and stats JSON.
    fn record_chat(
        &mut self,
        import_id: &str,
        filename: &str,
        hash: &str,
        imported_at: &str,
        stats_json: &str,
    ) -> Result<()>;

    /// The earliest chat import recorded for content `hash` (ties by id), if
    /// any.
    fn chat_import_with_hash(&self, hash: &str) -> Result<Option<String>>;

    /// Live memories written by the chat import `import_id`, or by any
    /// recorded chat import when `None`, by id.
    fn live_chat_memories(&self, import_id: Option<&str>) -> Result<Vec<String>>;

    /// Of the chat imports `import_ids`, those with no live memory left, by
    /// id.
    fn chat_imports_with_nothing_left(&self, import_ids: &[String]) -> Result<Vec<String>>;

    /// Drop the tracking rows of those chat imports `import_ids` with no
    /// live memory left. Returns how many went.
    fn forget_chat_imports_with_nothing_left(&mut self, import_ids: &[String]) -> Result<usize>;

    /// What earlier imports recorded for `external_ids` of `source`.
    fn dbs_tracked(
        &self,
        source: &str,
        external_ids: &[&str],
    ) -> Result<HashMap<DbsKey, DbsTracked>>;

    /// Record that `external_id` of `source` is now `memory_id` at
    /// `content_hash`, replacing what an earlier import recorded.
    fn record_dbs(
        &mut self,
        source: &str,
        external_id: &str,
        memory_id: &str,
        content_hash: &str,
        imported_at: &str,
    ) -> Result<()>;

    /// Live memories a dbs import recorded, from sources starting with
    /// `source_prefix`, or from any source when `None`, by memory id.
    fn live_dbs_memories(&self, source_prefix: Option<&str>) -> Result<Vec<String>>;

    /// Of `drawer_ids`, those already imported, sorted.
    fn imported_drawers(&self, drawer_ids: &[&str]) -> Result<Vec<String>>;

    /// Record that `drawer_id` was imported as `memory_id`. A drawer already
    /// recorded is left as it is.
    fn record_mempalace(
        &mut self,
        drawer_id: &str,
        memory_id: &str,
        imported_at: &str,
    ) -> Result<()>;

    /// Live memories a mempalace import recorded, from drawers starting with
    /// `drawer_prefix`, or from any drawer when `None`, by memory id.
    fn live_tracked_mempalace_memories(&self, drawer_prefix: Option<&str>) -> Result<Vec<String>>;

    /// Live memories that carry a mempalace source whether or not a
    /// tracking row records them, from drawers starting with
    /// `drawer_prefix`, or from any drawer when `None`, by id.
    fn live_mempalace_shaped_memories(&self, drawer_prefix: Option<&str>) -> Result<Vec<String>>;

    /// The distinct `doc_id`s of `memory_ids`, sorted.
    fn doc_ids_of(&self, memory_ids: &[String]) -> Result<Vec<String>>;

    /// Drop the dbs tracking rows for `memory_ids`. Returns how many went.
    fn forget_dbs(&mut self, memory_ids: &[String]) -> Result<usize>;

    /// Drop the mempalace tracking rows for `memory_ids`. Returns how many
    /// went.
    fn forget_mempalace(&mut self, memory_ids: &[String]) -> Result<usize>;
}

/// The import bookkeeping tables, on the engine's memories core.
pub struct ImportLedger<'c> {
    backend: &'c BackendLock,
}

impl<'c> ImportLedger<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            backend: store.backend(),
        }
    }

    /// See [`ImportStore::record_chat`].
    pub fn record_chat(
        &self,
        import_id: &str,
        filename: &str,
        hash: &str,
        imported_at: &str,
        stats_json: &str,
    ) -> Result<()> {
        ImportStore::record_chat(
            &mut *self.backend.lock()?,
            import_id,
            filename,
            hash,
            imported_at,
            stats_json,
        )
    }

    /// See [`ImportStore::chat_import_with_hash`].
    pub fn chat_import_with_hash(&self, hash: &str) -> Result<Option<String>> {
        ImportStore::chat_import_with_hash(&*self.backend.lock()?, hash)
    }

    /// See [`ImportStore::live_chat_memories`].
    pub fn live_chat_memories(&self, import_id: Option<&str>) -> Result<Vec<String>> {
        ImportStore::live_chat_memories(&*self.backend.lock()?, import_id)
    }

    /// See [`ImportStore::chat_imports_with_nothing_left`].
    pub fn chat_imports_with_nothing_left(&self, import_ids: &[String]) -> Result<Vec<String>> {
        ImportStore::chat_imports_with_nothing_left(&*self.backend.lock()?, import_ids)
    }

    /// See [`ImportStore::forget_chat_imports_with_nothing_left`].
    pub fn forget_chat_imports_with_nothing_left(&self, import_ids: &[String]) -> Result<usize> {
        ImportStore::forget_chat_imports_with_nothing_left(&mut *self.backend.lock()?, import_ids)
    }

    /// See [`ImportStore::dbs_tracked`].
    pub fn dbs_tracked(
        &self,
        source: &str,
        external_ids: &[&str],
    ) -> Result<HashMap<DbsKey, DbsTracked>> {
        ImportStore::dbs_tracked(&*self.backend.lock()?, source, external_ids)
    }

    /// See [`ImportStore::record_dbs`].
    pub fn record_dbs(
        &self,
        source: &str,
        external_id: &str,
        memory_id: &str,
        content_hash: &str,
        imported_at: &str,
    ) -> Result<()> {
        ImportStore::record_dbs(
            &mut *self.backend.lock()?,
            source,
            external_id,
            memory_id,
            content_hash,
            imported_at,
        )
    }

    /// See [`ImportStore::live_dbs_memories`].
    pub fn live_dbs_memories(&self, source_prefix: Option<&str>) -> Result<Vec<String>> {
        ImportStore::live_dbs_memories(&*self.backend.lock()?, source_prefix)
    }

    /// See [`ImportStore::imported_drawers`].
    pub fn imported_drawers(&self, drawer_ids: &[&str]) -> Result<Vec<String>> {
        ImportStore::imported_drawers(&*self.backend.lock()?, drawer_ids)
    }

    /// See [`ImportStore::record_mempalace`].
    pub fn record_mempalace(
        &self,
        drawer_id: &str,
        memory_id: &str,
        imported_at: &str,
    ) -> Result<()> {
        ImportStore::record_mempalace(
            &mut *self.backend.lock()?,
            drawer_id,
            memory_id,
            imported_at,
        )
    }

    /// See [`ImportStore::live_tracked_mempalace_memories`].
    pub fn live_tracked_mempalace_memories(
        &self,
        drawer_prefix: Option<&str>,
    ) -> Result<Vec<String>> {
        ImportStore::live_tracked_mempalace_memories(&*self.backend.lock()?, drawer_prefix)
    }

    /// See [`ImportStore::live_mempalace_shaped_memories`].
    pub fn live_mempalace_shaped_memories(
        &self,
        drawer_prefix: Option<&str>,
    ) -> Result<Vec<String>> {
        ImportStore::live_mempalace_shaped_memories(&*self.backend.lock()?, drawer_prefix)
    }

    /// See [`ImportStore::doc_ids_of`].
    pub fn doc_ids_of(&self, memory_ids: &[String]) -> Result<Vec<String>> {
        ImportStore::doc_ids_of(&*self.backend.lock()?, memory_ids)
    }

    /// See [`ImportStore::forget_dbs`].
    pub fn forget_dbs(&self, memory_ids: &[String]) -> Result<usize> {
        ImportStore::forget_dbs(&mut *self.backend.lock()?, memory_ids)
    }

    /// See [`ImportStore::forget_mempalace`].
    pub fn forget_mempalace(&self, memory_ids: &[String]) -> Result<usize> {
        ImportStore::forget_mempalace(&mut *self.backend.lock()?, memory_ids)
    }
}

/// The behaviour every [`ImportStore`] must show, run against each backend
/// by [`import_conformance`] (ADR-0035).
///
/// An import's reads are over the memories it wrote, so a backend under
/// test implements [`MemoryStore`] and, for the import count,
/// [`StatsStore`] too.
#[cfg(test)]
pub(crate) mod conformance {
    use super::{DbsTracked, ImportStore};
    use crate::db::memories::{MemoryStore, NewMemory};
    use crate::db::stats::StatsStore;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    /// Every read and write of the import ledger on a fixed corpus.
    pub(crate) fn the_ledger_tracks_chat_dbs_and_mempalace_imports<
        S: ImportStore + MemoryStore + StatsStore,
    >(
        store: &mut S,
    ) {
        let doc = |id: &str, doc: &str| NewMemory {
            doc_id: Some(doc.to_string()),
            ..NewMemory::new(id, "x", NOW)
        };
        MemoryStore::insert(store, &doc("m2", "imp_a")).unwrap();
        MemoryStore::insert(store, &doc("m1", "imp_a")).unwrap();
        MemoryStore::insert(store, &doc("m3", "imp_c")).unwrap();
        MemoryStore::insert(store, &doc("m9", "not_a_chat")).unwrap();
        MemoryStore::insert(
            store,
            &NewMemory {
                deleted_at: Some(NOW.to_string()),
                ..doc("m4", "imp_b")
            },
        )
        .unwrap();
        let palace = |id: &str, source: &str, drawer: serde_json::Value| NewMemory {
            source: source.to_string(),
            metadata: serde_json::json!({ "mempalace_drawer_id": drawer }),
            ..NewMemory::new(id, "p", NOW)
        };
        MemoryStore::insert(store, &palace("p1", "mempalace_import", "Wing_1".into())).unwrap();
        MemoryStore::insert(store, &palace("p2", "MemPalace:notes", "wingX".into())).unwrap();
        MemoryStore::insert(store, &palace("p3", "mempalace:notes", 17.into())).unwrap();
        MemoryStore::insert(store, &palace("p4", "Mempalace_import", "wing_2".into())).unwrap();
        MemoryStore::insert(
            store,
            &NewMemory {
                source: "mempalace:other".to_string(),
                ..NewMemory::new("p5", "p", NOW)
            },
        )
        .unwrap();
        MemoryStore::insert(store, &NewMemory::new("d1", "x", NOW)).unwrap();
        MemoryStore::insert(store, &NewMemory::new("d2", "x", NOW)).unwrap();
        MemoryStore::insert(
            store,
            &NewMemory {
                deleted_at: Some(NOW.to_string()),
                ..NewMemory::new("d3", "x", NOW)
            },
        )
        .unwrap();

        store
            .record_chat("imp_a", "a.json", "h1", "2026-02-02", "{}")
            .unwrap();
        store
            .record_chat("imp_b", "b.json", "h2", NOW, "{}")
            .unwrap();
        store
            .record_chat("imp_c", "c.json", "h1", "2026-01-01", "{}")
            .unwrap();
        store
            .record_chat("imp_d", "d.json", "h3", NOW, "{}")
            .unwrap();
        assert!(
            store
                .record_chat("imp_a", "again.json", "h9", NOW, "{}")
                .is_err(),
            "a recorded chat import is refused"
        );

        store.record_dbs("Src_1", "e1", "d1", "h1", NOW).unwrap();
        store.record_dbs("src-2", "e1", "d2", "h1", NOW).unwrap();
        store.record_dbs("SRCx", "e2", "d3", "h1", NOW).unwrap();
        store.record_dbs("Src_1", "e1", "d2", "h2", NOW).unwrap();
        store.record_dbs("Src_1", "e3", "gone", "h1", NOW).unwrap();

        store.record_mempalace("Wing_1", "p1", NOW).unwrap();
        store.record_mempalace("wing_2", "p4", NOW).unwrap();
        store.record_mempalace("wing_2", "p2", NOW).unwrap();
        store.record_mempalace("wingZ", "d3", NOW).unwrap();

        let chats: Vec<String> = ["imp_d", "imp_a", "imp_b", "imp_b", "none"]
            .map(String::from)
            .to_vec();
        let owned = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            store.chat_import_with_hash("h1").unwrap().as_deref(),
            Some("imp_c"),
            "the earliest import of the hash"
        );
        assert_eq!(store.chat_import_with_hash("h9").unwrap(), None);
        assert_eq!(
            store.live_chat_memories(Some("imp_a")).unwrap(),
            ["m1", "m2"]
        );
        assert_eq!(
            store.live_chat_memories(None).unwrap(),
            ["m1", "m2", "m3"],
            "m4 is deleted and m9's doc is no chat import"
        );
        assert_eq!(
            store.chat_imports_with_nothing_left(&chats).unwrap(),
            ["imp_b", "imp_d"]
        );
        let mut tracked: Vec<String> = store
            .dbs_tracked("Src_1", &["e1", "e3", "e9"])
            .unwrap()
            .into_iter()
            .map(|(k, v)| format!("{}/{}={}@{}", k.0, k.1, v.memory_id, v.content_hash))
            .collect();
        tracked.sort();
        assert_eq!(
            tracked,
            ["Src_1/e1=d2@h2", "Src_1/e3=gone@h1"],
            "a rerun replaces the tracked memory"
        );
        assert_eq!(store.live_dbs_memories(None).unwrap(), ["d2", "d2"]);
        assert_eq!(
            store.live_dbs_memories(Some("src_")).unwrap(),
            ["d2", "d2"],
            "the prefix is a LIKE pattern: case-insensitive, `_` any one character"
        );
        assert_eq!(store.live_dbs_memories(Some("src-")).unwrap(), ["d2"]);
        assert_eq!(
            store.imported_drawers(&["wing_2", "Wing_1", "x"]).unwrap(),
            ["Wing_1", "wing_2"]
        );
        assert_eq!(
            store.live_tracked_mempalace_memories(None).unwrap(),
            ["p1", "p4"],
            "wing_2's second record was ignored, wingZ's memory is deleted"
        );
        assert_eq!(
            store
                .live_tracked_mempalace_memories(Some("WING_"))
                .unwrap(),
            ["p1", "p4"]
        );
        assert_eq!(
            store.live_mempalace_shaped_memories(None).unwrap(),
            ["p1", "p2", "p3", "p5"],
            "`mempalace:` sources match case-insensitively; `mempalace_import` exactly"
        );
        assert_eq!(
            store.live_mempalace_shaped_memories(Some("wing_")).unwrap(),
            ["p1", "p2"]
        );
        assert_eq!(
            store.live_mempalace_shaped_memories(Some("1")).unwrap(),
            ["p3"],
            "a numeric drawer id is matched as text"
        );
        assert_eq!(
            store
                .doc_ids_of(&owned(&["m1", "m2", "m4", "d1", "zz"]))
                .unwrap(),
            ["imp_a", "imp_b"]
        );
        assert_eq!(StatsStore::imports(store).unwrap(), 4);
        assert_eq!(
            store.forget_chat_imports_with_nothing_left(&chats).unwrap(),
            2
        );
        assert_eq!(store.forget_dbs(&owned(&["d2", "d2", "d1"])).unwrap(), 2);
        assert_eq!(store.forget_mempalace(&owned(&["p4", "p9"])).unwrap(), 1);
        assert_eq!(store.live_chat_memories(None).unwrap(), ["m1", "m2", "m3"]);
        assert!(store.live_dbs_memories(None).unwrap().is_empty());
        assert_eq!(
            store.imported_drawers(&["wing_2", "Wing_1"]).unwrap(),
            ["Wing_1"]
        );
        assert_eq!(StatsStore::imports(store).unwrap(), 2);
    }

    pub(crate) fn a_chat_import_is_forgotten_only_once_nothing_of_it_is_left<
        S: ImportStore + MemoryStore,
    >(
        store: &mut S,
    ) {
        store
            .record_chat("imp_a", "a.json", "ha", NOW, "{}")
            .unwrap();
        store
            .record_chat("imp_b", "b.json", "hb", NOW, "{}")
            .unwrap();
        MemoryStore::insert(
            store,
            &NewMemory {
                doc_id: Some("imp_a".to_string()),
                ..NewMemory::new("m1", "x", NOW)
            },
        )
        .unwrap();

        let both = vec!["imp_a".to_string(), "imp_b".to_string()];
        assert_eq!(
            store.chat_imports_with_nothing_left(&both).unwrap(),
            vec!["imp_b"]
        );
        assert_eq!(
            store.forget_chat_imports_with_nothing_left(&both).unwrap(),
            1
        );
        assert_eq!(
            store.chat_import_with_hash("ha").unwrap().as_deref(),
            Some("imp_a")
        );
        assert_eq!(store.chat_import_with_hash("hb").unwrap(), None);
    }

    pub(crate) fn a_dbs_rerun_replaces_the_tracked_memory(store: &mut dyn ImportStore) {
        store.record_dbs("src", "e1", "m1", "h1", NOW).unwrap();
        store.record_dbs("src", "e1", "m2", "h2", NOW).unwrap();

        let tracked = store.dbs_tracked("src", &["e1", "e2"]).unwrap();
        assert_eq!(tracked.len(), 1);
        assert_eq!(
            tracked[&("src".to_string(), "e1".to_string())],
            DbsTracked {
                memory_id: "m2".to_string(),
                content_hash: "h2".to_string(),
            }
        );
        assert!(store.dbs_tracked("src", &[]).unwrap().is_empty());
    }

    pub(crate) fn a_recorded_drawer_is_not_rerecorded(store: &mut dyn ImportStore) {
        store.record_mempalace("d1", "m1", NOW).unwrap();
        store.record_mempalace("d1", "m2", NOW).unwrap();
        assert_eq!(store.imported_drawers(&["d1", "d2"]).unwrap(), vec!["d1"]);
        assert_eq!(store.forget_mempalace(&["m1".to_string()]).unwrap(), 1);
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`.
#[cfg(test)]
macro_rules! import_conformance {
    ($make:expr) => {
        use $crate::db::imports::conformance;

        #[test]
        fn the_ledger_tracks_chat_dbs_and_mempalace_imports() {
            conformance::the_ledger_tracks_chat_dbs_and_mempalace_imports(&mut $make);
        }
        #[test]
        fn a_chat_import_is_forgotten_only_once_nothing_of_it_is_left() {
            conformance::a_chat_import_is_forgotten_only_once_nothing_of_it_is_left(&mut $make);
        }
        #[test]
        fn a_dbs_rerun_replaces_the_tracked_memory() {
            conformance::a_dbs_rerun_replaces_the_tracked_memory(&mut $make);
        }
        #[test]
        fn a_recorded_drawer_is_not_rerecorded() {
            conformance::a_recorded_drawer_is_not_rerecorded(&mut $make);
        }
    };
}
#[cfg(test)]
pub(crate) use import_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    /// The facade locks per call and reaches the engine through the port.
    #[test]
    fn the_facade_reads_back_what_it_records() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let ledger = ImportLedger::new(&store);
        ledger
            .record_chat("imp_a", "a.json", "h", "2026-09-26", "{}")
            .unwrap();
        ledger
            .record_dbs("src", "e1", "m1", "h1", "2026-09-26")
            .unwrap();
        ledger.record_mempalace("d1", "m2", "2026-09-26").unwrap();
        assert_eq!(
            ledger.chat_import_with_hash("h").unwrap().as_deref(),
            Some("imp_a")
        );
        assert_eq!(ledger.dbs_tracked("src", &["e1"]).unwrap().len(), 1);
        assert_eq!(ledger.imported_drawers(&["d1"]).unwrap(), ["d1"]);
        assert_eq!(ledger.forget_dbs(&["m1".to_string()]).unwrap(), 1);
    }
}
