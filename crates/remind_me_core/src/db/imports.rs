//! Storage for import bookkeeping: `chat_imports` (one row per imported
//! file), `dbs_imports` (one row per daily-backup-system item) and
//! `mempalace_imports` (one row per drawer), plus the reads that find what
//! an import wrote so it can be undone, on the engine's memories core
//! (`db::engine::imports`).
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

use super::engine::{self, EngineLock};
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

/// The import bookkeeping tables, on the engine's memories core.
pub struct ImportLedger<'c> {
    core: &'c EngineLock,
}

impl<'c> ImportLedger<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    // --- chat imports ----------------------------------------------------

    /// Record the chat import `import_id` of `filename`, with its content
    /// hash and stats JSON.
    pub fn record_chat(
        &self,
        import_id: &str,
        filename: &str,
        hash: &str,
        imported_at: &str,
        stats_json: &str,
    ) -> Result<()> {
        engine::imports::record_chat(
            &mut self.core.lock(),
            import_id,
            filename,
            hash,
            imported_at,
            stats_json,
        )
    }

    /// The earliest chat import recorded for content `hash` (ties by id), if
    /// any.
    pub fn chat_import_with_hash(&self, hash: &str) -> Result<Option<String>> {
        engine::imports::chat_import_with_hash(&self.core.lock(), hash)
    }

    /// Live memories written by the chat import `import_id`, or by any
    /// recorded chat import when `None`, by id.
    pub fn live_chat_memories(&self, import_id: Option<&str>) -> Result<Vec<String>> {
        engine::imports::live_chat_memories(&self.core.lock(), import_id)
    }

    /// Of the chat imports `import_ids`, those with no live memory left, by
    /// id.
    pub fn chat_imports_with_nothing_left(&self, import_ids: &[String]) -> Result<Vec<String>> {
        engine::imports::chat_imports_with_nothing_left(&self.core.lock(), import_ids)
    }

    /// Drop the tracking rows of those chat imports `import_ids` with no
    /// live memory left. Returns how many went.
    pub fn forget_chat_imports_with_nothing_left(&self, import_ids: &[String]) -> Result<usize> {
        engine::imports::forget_chat_imports_with_nothing_left(&mut self.core.lock(), import_ids)
    }

    // --- dbs imports -----------------------------------------------------

    /// What earlier imports recorded for `external_ids` of `source`.
    pub fn dbs_tracked(
        &self,
        source: &str,
        external_ids: &[&str],
    ) -> Result<HashMap<DbsKey, DbsTracked>> {
        engine::imports::dbs_tracked(&self.core.lock(), source, external_ids)
    }

    /// Record that `external_id` of `source` is now `memory_id` at
    /// `content_hash`, replacing what an earlier import recorded.
    pub fn record_dbs(
        &self,
        source: &str,
        external_id: &str,
        memory_id: &str,
        content_hash: &str,
        imported_at: &str,
    ) -> Result<()> {
        engine::imports::record_dbs(
            &mut self.core.lock(),
            source,
            external_id,
            memory_id,
            content_hash,
            imported_at,
        )
    }

    /// Live memories a dbs import recorded, from sources starting with
    /// `source_prefix`, or from any source when `None`, by memory id.
    pub fn live_dbs_memories(&self, source_prefix: Option<&str>) -> Result<Vec<String>> {
        engine::imports::live_dbs_memories(&self.core.lock(), source_prefix)
    }

    // --- mempalace imports -----------------------------------------------

    /// Of `drawer_ids`, those already imported, sorted.
    pub fn imported_drawers(&self, drawer_ids: &[&str]) -> Result<Vec<String>> {
        engine::imports::imported_drawers(&self.core.lock(), drawer_ids)
    }

    /// Record that `drawer_id` was imported as `memory_id`. A drawer already
    /// recorded is left as it is.
    pub fn record_mempalace(
        &self,
        drawer_id: &str,
        memory_id: &str,
        imported_at: &str,
    ) -> Result<()> {
        engine::imports::record_mempalace(&mut self.core.lock(), drawer_id, memory_id, imported_at)
    }

    /// Live memories a mempalace import recorded, from drawers starting with
    /// `drawer_prefix`, or from any drawer when `None`, by memory id.
    pub fn live_tracked_mempalace_memories(
        &self,
        drawer_prefix: Option<&str>,
    ) -> Result<Vec<String>> {
        engine::imports::live_tracked_mempalace_memories(&self.core.lock(), drawer_prefix)
    }

    /// Live memories that carry a mempalace source whether or not a
    /// tracking row records them, from drawers starting with
    /// `drawer_prefix`, or from any drawer when `None`, by id.
    pub fn live_mempalace_shaped_memories(
        &self,
        drawer_prefix: Option<&str>,
    ) -> Result<Vec<String>> {
        engine::imports::live_mempalace_shaped_memories(&self.core.lock(), drawer_prefix)
    }

    // --- undo ------------------------------------------------------------

    /// The distinct `doc_id`s of `memory_ids`, sorted.
    pub fn doc_ids_of(&self, memory_ids: &[String]) -> Result<Vec<String>> {
        engine::imports::doc_ids_of(&self.core.lock(), memory_ids)
    }

    /// Drop the dbs tracking rows for `memory_ids`. Returns how many went.
    pub fn forget_dbs(&self, memory_ids: &[String]) -> Result<usize> {
        engine::imports::forget_dbs(&mut self.core.lock(), memory_ids)
    }

    /// Drop the mempalace tracking rows for `memory_ids`. Returns how many
    /// went.
    pub fn forget_mempalace(&self, memory_ids: &[String]) -> Result<usize> {
        engine::imports::forget_mempalace(&mut self.core.lock(), memory_ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::stats::StoreStats;
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    /// Every read and write of the import ledger on a fixed corpus.
    #[test]
    fn the_ledger_tracks_chat_dbs_and_mempalace_imports() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        let doc = |id: &str, doc: &str| NewMemory {
            doc_id: Some(doc.to_string()),
            ..NewMemory::new(id, "x", NOW)
        };
        memories.insert(&doc("m2", "imp_a")).unwrap();
        memories.insert(&doc("m1", "imp_a")).unwrap();
        memories.insert(&doc("m3", "imp_c")).unwrap();
        memories.insert(&doc("m9", "not_a_chat")).unwrap();
        memories
            .insert(&NewMemory {
                deleted_at: Some(NOW.to_string()),
                ..doc("m4", "imp_b")
            })
            .unwrap();
        let palace = |id: &str, source: &str, drawer: serde_json::Value| NewMemory {
            source: source.to_string(),
            metadata: serde_json::json!({ "mempalace_drawer_id": drawer }),
            ..NewMemory::new(id, "p", NOW)
        };
        memories
            .insert(&palace("p1", "mempalace_import", "Wing_1".into()))
            .unwrap();
        memories
            .insert(&palace("p2", "MemPalace:notes", "wingX".into()))
            .unwrap();
        memories
            .insert(&palace("p3", "mempalace:notes", 17.into()))
            .unwrap();
        memories
            .insert(&palace("p4", "Mempalace_import", "wing_2".into()))
            .unwrap();
        memories
            .insert(&NewMemory {
                source: "mempalace:other".to_string(),
                ..NewMemory::new("p5", "p", NOW)
            })
            .unwrap();
        memories.insert(&NewMemory::new("d1", "x", NOW)).unwrap();
        memories.insert(&NewMemory::new("d2", "x", NOW)).unwrap();
        memories
            .insert(&NewMemory {
                deleted_at: Some(NOW.to_string()),
                ..NewMemory::new("d3", "x", NOW)
            })
            .unwrap();

        let ledger = ImportLedger::new(&store);
        ledger
            .record_chat("imp_a", "a.json", "h1", "2026-02-02", "{}")
            .unwrap();
        ledger
            .record_chat("imp_b", "b.json", "h2", NOW, "{}")
            .unwrap();
        ledger
            .record_chat("imp_c", "c.json", "h1", "2026-01-01", "{}")
            .unwrap();
        ledger
            .record_chat("imp_d", "d.json", "h3", NOW, "{}")
            .unwrap();
        assert!(
            ledger
                .record_chat("imp_a", "again.json", "h9", NOW, "{}")
                .is_err(),
            "a recorded chat import is refused"
        );

        ledger.record_dbs("Src_1", "e1", "d1", "h1", NOW).unwrap();
        ledger.record_dbs("src-2", "e1", "d2", "h1", NOW).unwrap();
        ledger.record_dbs("SRCx", "e2", "d3", "h1", NOW).unwrap();
        ledger.record_dbs("Src_1", "e1", "d2", "h2", NOW).unwrap();
        ledger.record_dbs("Src_1", "e3", "gone", "h1", NOW).unwrap();

        ledger.record_mempalace("Wing_1", "p1", NOW).unwrap();
        ledger.record_mempalace("wing_2", "p4", NOW).unwrap();
        ledger.record_mempalace("wing_2", "p2", NOW).unwrap();
        ledger.record_mempalace("wingZ", "d3", NOW).unwrap();

        let chats: Vec<String> = ["imp_d", "imp_a", "imp_b", "imp_b", "none"]
            .map(String::from)
            .to_vec();
        let owned = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            ledger.chat_import_with_hash("h1").unwrap().as_deref(),
            Some("imp_c"),
            "the earliest import of the hash"
        );
        assert_eq!(ledger.chat_import_with_hash("h9").unwrap(), None);
        assert_eq!(
            ledger.live_chat_memories(Some("imp_a")).unwrap(),
            ["m1", "m2"]
        );
        assert_eq!(
            ledger.live_chat_memories(None).unwrap(),
            ["m1", "m2", "m3"],
            "m4 is deleted and m9's doc is no chat import"
        );
        assert_eq!(
            ledger.chat_imports_with_nothing_left(&chats).unwrap(),
            ["imp_b", "imp_d"]
        );
        let mut tracked: Vec<String> = ledger
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
        assert_eq!(ledger.live_dbs_memories(None).unwrap(), ["d2", "d2"]);
        assert_eq!(
            ledger.live_dbs_memories(Some("src_")).unwrap(),
            ["d2", "d2"],
            "the prefix is a LIKE pattern: case-insensitive, `_` any one character"
        );
        assert_eq!(ledger.live_dbs_memories(Some("src-")).unwrap(), ["d2"]);
        assert_eq!(
            ledger.imported_drawers(&["wing_2", "Wing_1", "x"]).unwrap(),
            ["Wing_1", "wing_2"]
        );
        assert_eq!(
            ledger.live_tracked_mempalace_memories(None).unwrap(),
            ["p1", "p4"],
            "wing_2's second record was ignored, wingZ's memory is deleted"
        );
        assert_eq!(
            ledger
                .live_tracked_mempalace_memories(Some("WING_"))
                .unwrap(),
            ["p1", "p4"]
        );
        assert_eq!(
            ledger.live_mempalace_shaped_memories(None).unwrap(),
            ["p1", "p2", "p3", "p5"],
            "`mempalace:` sources match case-insensitively; `mempalace_import` exactly"
        );
        assert_eq!(
            ledger
                .live_mempalace_shaped_memories(Some("wing_"))
                .unwrap(),
            ["p1", "p2"]
        );
        assert_eq!(
            ledger.live_mempalace_shaped_memories(Some("1")).unwrap(),
            ["p3"],
            "a numeric drawer id is matched as text"
        );
        assert_eq!(
            ledger
                .doc_ids_of(&owned(&["m1", "m2", "m4", "d1", "zz"]))
                .unwrap(),
            ["imp_a", "imp_b"]
        );
        assert_eq!(StoreStats::new(&store).imports().unwrap(), 4);
        assert_eq!(
            ledger
                .forget_chat_imports_with_nothing_left(&chats)
                .unwrap(),
            2
        );
        assert_eq!(
            ledger.forget_dbs(&owned(&["d2", "d2", "d1"])).unwrap(),
            2
        );
        assert_eq!(
            ledger.forget_mempalace(&owned(&["p4", "p9"])).unwrap(),
            1
        );
        assert_eq!(ledger.live_chat_memories(None).unwrap(), ["m1", "m2", "m3"]);
        assert!(ledger.live_dbs_memories(None).unwrap().is_empty());
        assert_eq!(
            ledger.imported_drawers(&["wing_2", "Wing_1"]).unwrap(),
            ["Wing_1"]
        );
        assert_eq!(StoreStats::new(&store).imports().unwrap(), 2);
    }

    #[test]
    fn a_chat_import_is_forgotten_only_once_nothing_of_it_is_left() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let ledger = ImportLedger::new(&store);
        ledger
            .record_chat("imp_a", "a.json", "ha", NOW, "{}")
            .unwrap();
        ledger
            .record_chat("imp_b", "b.json", "hb", NOW, "{}")
            .unwrap();
        Memories::new(&store)
            .insert(&NewMemory {
                doc_id: Some("imp_a".to_string()),
                ..NewMemory::new("m1", "x", NOW)
            })
            .unwrap();

        let both = vec!["imp_a".to_string(), "imp_b".to_string()];
        assert_eq!(
            ledger.chat_imports_with_nothing_left(&both).unwrap(),
            vec!["imp_b"]
        );
        assert_eq!(
            ledger.forget_chat_imports_with_nothing_left(&both).unwrap(),
            1
        );
        assert_eq!(
            ledger.chat_import_with_hash("ha").unwrap().as_deref(),
            Some("imp_a")
        );
        assert_eq!(ledger.chat_import_with_hash("hb").unwrap(), None);
    }

    #[test]
    fn a_dbs_rerun_replaces_the_tracked_memory() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let ledger = ImportLedger::new(&store);
        ledger.record_dbs("src", "e1", "m1", "h1", NOW).unwrap();
        ledger.record_dbs("src", "e1", "m2", "h2", NOW).unwrap();

        let tracked = ledger.dbs_tracked("src", &["e1", "e2"]).unwrap();
        assert_eq!(tracked.len(), 1);
        assert_eq!(
            tracked[&("src".to_string(), "e1".to_string())],
            DbsTracked {
                memory_id: "m2".to_string(),
                content_hash: "h2".to_string(),
            }
        );
        assert!(ledger.dbs_tracked("src", &[]).unwrap().is_empty());
    }

    #[test]
    fn a_recorded_drawer_is_not_rerecorded() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let ledger = ImportLedger::new(&store);
        ledger.record_mempalace("d1", "m1", NOW).unwrap();
        ledger.record_mempalace("d1", "m2", NOW).unwrap();
        assert_eq!(ledger.imported_drawers(&["d1", "d2"]).unwrap(), vec!["d1"]);
        assert_eq!(ledger.forget_mempalace(&["m1".to_string()]).unwrap(), 1);
    }
}
