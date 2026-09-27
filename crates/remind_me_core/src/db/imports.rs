//! Storage for import bookkeeping: `chat_imports` (one row per imported
//! file), `dbs_imports` (one row per daily-backup-system item) and
//! `mempalace_imports` (one row per drawer), plus the reads that find what
//! an import wrote so it can be undone.
//!
//! ADR-0023 phase 1, step 5b. Every statement against these tables from
//! [`crate::importer`], [`crate::dbs_import`], [`crate::mempalace_import`]
//! and [`crate::undo_import`] lives here. Reading the foreign SQLite files
//! those importers take in is not storage, so it stays with them.
//!
//! The rules stay with the modules: when content counts as already
//! imported, when a changed dbs item supersedes its memory, and that a chat
//! import loses its tracking row only once nothing of it is left.
//!
//! With the `engine-store` feature, a store whose tables hold the memories
//! core keeps all three tables there (`db::engine::imports`, core PR 4a).

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineTables};
use super::{Result, Store};
#[cfg(feature = "engine-store")]
use parking_lot::Mutex;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use std::collections::HashMap;

/// What a previous import recorded for one dbs item.
#[derive(Debug, Clone, PartialEq)]
pub struct DbsTracked {
    pub memory_id: String,
    pub content_hash: String,
}

/// A dbs item's key: its source and its id there.
pub type DbsKey = (String, String);

/// Live memories whose `doc_id` is a chat import's id, not yet deleted.
const LIVE_CHAT_DOC_IDS: &str = "SELECT doc_id FROM memories
                 WHERE doc_id IS NOT NULL AND deleted_at IS NULL";

/// One placeholder per item, comma-separated.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// The import bookkeeping tables, over one connection, or on the engine's
/// memories core when the store's tables hold it (`db::engine::imports`).
pub struct ImportLedger<'c> {
    conn: &'c Connection,
    #[cfg(feature = "engine-store")]
    core: Option<&'c Mutex<EngineTables>>,
}

impl<'c> ImportLedger<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            #[cfg(feature = "engine-store")]
            core: store.core(),
        }
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
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::record_chat(
                &mut core.lock(),
                import_id,
                filename,
                hash,
                imported_at,
                stats_json,
            );
        }
        self.conn.execute(
            "INSERT INTO chat_imports (import_id, filename, hash, imported_at, stats)
             VALUES (?, ?, ?, ?, ?)",
            params![import_id, filename, hash, imported_at, stats_json],
        )?;
        Ok(())
    }

    /// The earliest chat import recorded for content `hash` (ties by id), if
    /// any.
    pub fn chat_import_with_hash(&self, hash: &str) -> Result<Option<String>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::chat_import_with_hash(&core.lock(), hash);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT import_id FROM chat_imports WHERE hash = ?
                  ORDER BY imported_at, import_id LIMIT 1",
                params![hash],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Live memories written by the chat import `import_id`, or by any
    /// recorded chat import when `None`, by id.
    pub fn live_chat_memories(&self, import_id: Option<&str>) -> Result<Vec<String>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::live_chat_memories(&core.lock(), import_id);
        }
        match import_id {
            Some(id) => self.ids(
                "SELECT id FROM memories WHERE doc_id = ? AND deleted_at IS NULL ORDER BY id",
                &[id.to_string()],
            ),
            None => self.ids(
                "SELECT id FROM memories
                  WHERE deleted_at IS NULL
                    AND doc_id IN (SELECT import_id FROM chat_imports)
                  ORDER BY id",
                &[],
            ),
        }
    }

    /// Of the chat imports `import_ids`, those with no live memory left, by
    /// id.
    pub fn chat_imports_with_nothing_left(&self, import_ids: &[String]) -> Result<Vec<String>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::chat_imports_with_nothing_left(&core.lock(), import_ids);
        }
        if import_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.ids(
            &format!(
                "SELECT import_id FROM chat_imports
                  WHERE import_id IN ({})
                    AND import_id NOT IN ({LIVE_CHAT_DOC_IDS})
                  ORDER BY import_id",
                placeholders(import_ids.len())
            ),
            import_ids,
        )
    }

    /// Drop the tracking rows of those chat imports `import_ids` with no
    /// live memory left. Returns how many went.
    pub fn forget_chat_imports_with_nothing_left(&self, import_ids: &[String]) -> Result<usize> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::forget_chat_imports_with_nothing_left(
                &mut core.lock(),
                import_ids,
            );
        }
        if import_ids.is_empty() {
            return Ok(0);
        }
        Ok(self.conn.execute(
            &format!(
                "DELETE FROM chat_imports
                  WHERE import_id IN ({})
                    AND import_id NOT IN ({LIVE_CHAT_DOC_IDS})",
                placeholders(import_ids.len())
            ),
            params_from_iter(import_ids.iter()),
        )?)
    }

    // --- dbs imports -----------------------------------------------------

    /// What earlier imports recorded for `external_ids` of `source`.
    pub fn dbs_tracked(
        &self,
        source: &str,
        external_ids: &[&str],
    ) -> Result<HashMap<DbsKey, DbsTracked>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::dbs_tracked(&core.lock(), source, external_ids);
        }
        if external_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = format!(
            "SELECT dbs_source, external_id, memory_id, content_hash
               FROM dbs_imports
              WHERE dbs_source = ? AND external_id IN ({})",
            placeholders(external_ids.len())
        );
        let mut values = vec![SqlValue::Text(source.to_string())];
        values.extend(
            external_ids
                .iter()
                .map(|id| SqlValue::Text((*id).to_string())),
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(values), |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    DbsTracked {
                        memory_id: row.get(2)?,
                        content_hash: row.get(3)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
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
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::record_dbs(
                &mut core.lock(),
                source,
                external_id,
                memory_id,
                content_hash,
                imported_at,
            );
        }
        self.conn.execute(
            "INSERT INTO dbs_imports (dbs_source, external_id, memory_id, content_hash, imported_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(dbs_source, external_id)
             DO UPDATE SET memory_id = excluded.memory_id,
                           content_hash = excluded.content_hash,
                           imported_at = excluded.imported_at",
            params![source, external_id, memory_id, content_hash, imported_at],
        )?;
        Ok(())
    }

    /// Live memories a dbs import recorded, from sources starting with
    /// `source_prefix`, or from any source when `None`, by memory id.
    pub fn live_dbs_memories(&self, source_prefix: Option<&str>) -> Result<Vec<String>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::live_dbs_memories(&core.lock(), source_prefix);
        }
        match source_prefix {
            Some(prefix) => self.ids(
                "SELECT t.memory_id FROM dbs_imports t
                   JOIN memories m ON m.id = t.memory_id
                  WHERE m.deleted_at IS NULL AND t.dbs_source LIKE ?
                  ORDER BY t.memory_id",
                &[format!("{prefix}%")],
            ),
            None => self.ids(
                "SELECT t.memory_id FROM dbs_imports t
                   JOIN memories m ON m.id = t.memory_id
                  WHERE m.deleted_at IS NULL
                  ORDER BY t.memory_id",
                &[],
            ),
        }
    }

    // --- mempalace imports -----------------------------------------------

    /// Of `drawer_ids`, those already imported, sorted.
    pub fn imported_drawers(&self, drawer_ids: &[&str]) -> Result<Vec<String>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::imported_drawers(&core.lock(), drawer_ids);
        }
        if drawer_ids.is_empty() {
            return Ok(Vec::new());
        }
        let owned: Vec<String> = drawer_ids.iter().map(|d| (*d).to_string()).collect();
        self.ids(
            &format!(
                "SELECT drawer_id FROM mempalace_imports WHERE drawer_id IN ({}) ORDER BY drawer_id",
                placeholders(drawer_ids.len())
            ),
            &owned,
        )
    }

    /// Record that `drawer_id` was imported as `memory_id`. A drawer already
    /// recorded is left as it is.
    pub fn record_mempalace(
        &self,
        drawer_id: &str,
        memory_id: &str,
        imported_at: &str,
    ) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::record_mempalace(
                &mut core.lock(),
                drawer_id,
                memory_id,
                imported_at,
            );
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO mempalace_imports (drawer_id, memory_id, imported_at)
             VALUES (?, ?, ?)",
            params![drawer_id, memory_id, imported_at],
        )?;
        Ok(())
    }

    /// Live memories a mempalace import recorded, from drawers starting with
    /// `drawer_prefix`, or from any drawer when `None`, by memory id.
    pub fn live_tracked_mempalace_memories(
        &self,
        drawer_prefix: Option<&str>,
    ) -> Result<Vec<String>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::live_tracked_mempalace_memories(&core.lock(), drawer_prefix);
        }
        match drawer_prefix {
            Some(prefix) => self.ids(
                "SELECT t.memory_id FROM mempalace_imports t
                   JOIN memories m ON m.id = t.memory_id
                  WHERE m.deleted_at IS NULL AND t.drawer_id LIKE ?
                  ORDER BY t.memory_id",
                &[format!("{prefix}%")],
            ),
            None => self.ids(
                "SELECT t.memory_id FROM mempalace_imports t
                   JOIN memories m ON m.id = t.memory_id
                  WHERE m.deleted_at IS NULL
                  ORDER BY t.memory_id",
                &[],
            ),
        }
    }

    /// Live memories that carry a mempalace source whether or not a
    /// tracking row records them, from drawers starting with
    /// `drawer_prefix`, or from any drawer when `None`, by id.
    pub fn live_mempalace_shaped_memories(
        &self,
        drawer_prefix: Option<&str>,
    ) -> Result<Vec<String>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::live_mempalace_shaped_memories(&core.lock(), drawer_prefix);
        }
        match drawer_prefix {
            Some(prefix) => self.ids(
                "SELECT id FROM memories
                  WHERE deleted_at IS NULL
                    AND (source = 'mempalace_import' OR source LIKE 'mempalace:%')
                    AND json_extract(metadata, '$.mempalace_drawer_id') LIKE ?
                  ORDER BY id",
                &[format!("{prefix}%")],
            ),
            None => self.ids(
                "SELECT id FROM memories
                  WHERE deleted_at IS NULL
                    AND (source = 'mempalace_import' OR source LIKE 'mempalace:%')
                  ORDER BY id",
                &[],
            ),
        }
    }

    // --- undo ------------------------------------------------------------

    /// The distinct `doc_id`s of `memory_ids`, sorted.
    pub fn doc_ids_of(&self, memory_ids: &[String]) -> Result<Vec<String>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::doc_ids_of(&core.lock(), memory_ids);
        }
        if memory_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.ids(
            &format!(
                "SELECT DISTINCT doc_id FROM memories
                  WHERE doc_id IS NOT NULL AND id IN ({})
                  ORDER BY doc_id",
                placeholders(memory_ids.len())
            ),
            memory_ids,
        )
    }

    /// Drop the dbs tracking rows for `memory_ids`. Returns how many went.
    pub fn forget_dbs(&self, memory_ids: &[String]) -> Result<usize> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::forget_dbs(&mut core.lock(), memory_ids);
        }
        self.forget_by_memory("dbs_imports", memory_ids)
    }

    /// Drop the mempalace tracking rows for `memory_ids`. Returns how many
    /// went.
    pub fn forget_mempalace(&self, memory_ids: &[String]) -> Result<usize> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::forget_mempalace(&mut core.lock(), memory_ids);
        }
        self.forget_by_memory("mempalace_imports", memory_ids)
    }

    fn forget_by_memory(&self, table: &str, memory_ids: &[String]) -> Result<usize> {
        if memory_ids.is_empty() {
            return Ok(0);
        }
        Ok(self.conn.execute(
            &format!(
                "DELETE FROM {table} WHERE memory_id IN ({})",
                placeholders(memory_ids.len())
            ),
            params_from_iter(memory_ids.iter()),
        )?)
    }

    /// The first column of every row `sql` returns, bound to `bindings`.
    fn ids(&self, sql: &str, bindings: &[String]) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params_from_iter(bindings.iter()), |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::stats::StoreStats;
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    /// Every read and write of the import ledger, as text, on `db`.
    fn exercise(db: &Database) -> Vec<String> {
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
        let duplicate = ledger.record_chat("imp_a", "again.json", "h9", NOW, "{}");

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
        let mut tracked: Vec<String> = ledger
            .dbs_tracked("Src_1", &["e1", "e3", "e9"])
            .unwrap()
            .into_iter()
            .map(|(k, v)| format!("{k:?}={v:?}"))
            .collect();
        tracked.sort();
        let mut seen = vec![
            format!("{}", duplicate.is_err()),
            format!("{:?}", ledger.chat_import_with_hash("h1").unwrap()),
            format!("{:?}", ledger.chat_import_with_hash("h9").unwrap()),
            format!("{:?}", ledger.live_chat_memories(Some("imp_a")).unwrap()),
            format!("{:?}", ledger.live_chat_memories(None).unwrap()),
            format!(
                "{:?}",
                ledger.chat_imports_with_nothing_left(&chats).unwrap()
            ),
            format!("{:?}", tracked),
            format!("{:?}", ledger.live_dbs_memories(None).unwrap()),
            format!("{:?}", ledger.live_dbs_memories(Some("src_")).unwrap()),
            format!(
                "{:?}",
                ledger.imported_drawers(&["wing_2", "Wing_1", "x"]).unwrap()
            ),
            format!(
                "{:?}",
                ledger.live_tracked_mempalace_memories(None).unwrap()
            ),
            format!(
                "{:?}",
                ledger
                    .live_tracked_mempalace_memories(Some("WING_"))
                    .unwrap()
            ),
            format!("{:?}", ledger.live_mempalace_shaped_memories(None).unwrap()),
            format!(
                "{:?}",
                ledger
                    .live_mempalace_shaped_memories(Some("wing_"))
                    .unwrap()
            ),
            format!(
                "{:?}",
                ledger.live_mempalace_shaped_memories(Some("1")).unwrap()
            ),
            format!(
                "{:?}",
                ledger
                    .doc_ids_of(&owned(&["m1", "m2", "m4", "d1", "zz"]))
                    .unwrap()
            ),
            format!("{}", StoreStats::new(&store).imports().unwrap()),
        ];
        seen.push(format!(
            "{}",
            ledger
                .forget_chat_imports_with_nothing_left(&chats)
                .unwrap()
        ));
        seen.push(format!(
            "{}",
            ledger.forget_dbs(&owned(&["d2", "d2", "d1"])).unwrap()
        ));
        seen.push(format!(
            "{}",
            ledger.forget_mempalace(&owned(&["p4", "p9"])).unwrap()
        ));
        seen.push(format!("{:?}", ledger.live_chat_memories(None).unwrap()));
        seen.push(format!("{:?}", ledger.live_dbs_memories(None).unwrap()));
        seen.push(format!(
            "{:?}",
            ledger.imported_drawers(&["wing_2", "Wing_1"]).unwrap()
        ));
        seen.push(format!("{}", StoreStats::new(&store).imports().unwrap()));
        seen
    }

    #[test]
    fn the_engine_core_keeps_the_import_ledger_as_sqlite_does() {
        let mut observed = Vec::new();
        crate::db::on_each_core_backend(|db| observed.push(exercise(db)));
        let sqlite = &observed[0];
        assert_eq!(sqlite[0], "true", "a recorded chat import is refused");
        for other in &observed[1..] {
            for (theirs, ours) in other.iter().zip(sqlite) {
                assert_eq!(theirs, ours);
            }
            assert_eq!(other.len(), sqlite.len());
        }
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
