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

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Result};
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

/// The import bookkeeping tables, over one connection.
pub struct ImportLedger<'c> {
    conn: &'c Connection,
}

impl<'c> ImportLedger<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
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
        self.conn.execute(
            "INSERT INTO chat_imports (import_id, filename, hash, imported_at, stats)
             VALUES (?, ?, ?, ?, ?)",
            params![import_id, filename, hash, imported_at, stats_json],
        )?;
        Ok(())
    }

    /// The chat import already recorded for content `hash`, if any.
    pub fn chat_import_with_hash(&self, hash: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT import_id FROM chat_imports WHERE hash = ?",
                params![hash],
                |r| r.get(0),
            )
            .optional()
    }

    /// Live memories written by the chat import `import_id`, or by any
    /// recorded chat import when `None`.
    pub fn live_chat_memories(&self, import_id: Option<&str>) -> Result<Vec<String>> {
        match import_id {
            Some(id) => self.ids(
                "SELECT id FROM memories WHERE doc_id = ? AND deleted_at IS NULL",
                &[id.to_string()],
            ),
            None => self.ids(
                "SELECT id FROM memories
                  WHERE deleted_at IS NULL
                    AND doc_id IN (SELECT import_id FROM chat_imports)",
                &[],
            ),
        }
    }

    /// Of the chat imports `import_ids`, those with no live memory left.
    pub fn chat_imports_with_nothing_left(&self, import_ids: &[String]) -> Result<Vec<String>> {
        if import_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.ids(
            &format!(
                "SELECT import_id FROM chat_imports
                  WHERE import_id IN ({})
                    AND import_id NOT IN ({LIVE_CHAT_DOC_IDS})",
                placeholders(import_ids.len())
            ),
            import_ids,
        )
    }

    /// Drop the tracking rows of those chat imports `import_ids` with no
    /// live memory left. Returns how many went.
    pub fn forget_chat_imports_with_nothing_left(&self, import_ids: &[String]) -> Result<usize> {
        if import_ids.is_empty() {
            return Ok(0);
        }
        self.conn.execute(
            &format!(
                "DELETE FROM chat_imports
                  WHERE import_id IN ({})
                    AND import_id NOT IN ({LIVE_CHAT_DOC_IDS})",
                placeholders(import_ids.len())
            ),
            params_from_iter(import_ids.iter()),
        )
    }

    // --- dbs imports -----------------------------------------------------

    /// What earlier imports recorded for `external_ids` of `source`.
    pub fn dbs_tracked(
        &self,
        source: &str,
        external_ids: &[&str],
    ) -> Result<HashMap<DbsKey, DbsTracked>> {
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
            .collect();
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
    /// `source_prefix`, or from any source when `None`.
    pub fn live_dbs_memories(&self, source_prefix: Option<&str>) -> Result<Vec<String>> {
        match source_prefix {
            Some(prefix) => self.ids(
                "SELECT t.memory_id FROM dbs_imports t
                   JOIN memories m ON m.id = t.memory_id
                  WHERE m.deleted_at IS NULL AND t.dbs_source LIKE ?",
                &[format!("{prefix}%")],
            ),
            None => self.ids(
                "SELECT t.memory_id FROM dbs_imports t
                   JOIN memories m ON m.id = t.memory_id
                  WHERE m.deleted_at IS NULL",
                &[],
            ),
        }
    }

    // --- mempalace imports -----------------------------------------------

    /// Of `drawer_ids`, those already imported.
    pub fn imported_drawers(&self, drawer_ids: &[&str]) -> Result<Vec<String>> {
        if drawer_ids.is_empty() {
            return Ok(Vec::new());
        }
        let owned: Vec<String> = drawer_ids.iter().map(|d| (*d).to_string()).collect();
        self.ids(
            &format!(
                "SELECT drawer_id FROM mempalace_imports WHERE drawer_id IN ({})",
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
        self.conn.execute(
            "INSERT OR IGNORE INTO mempalace_imports (drawer_id, memory_id, imported_at)
             VALUES (?, ?, ?)",
            params![drawer_id, memory_id, imported_at],
        )?;
        Ok(())
    }

    /// Live memories a mempalace import recorded, from drawers starting with
    /// `drawer_prefix`, or from any drawer when `None`.
    pub fn live_tracked_mempalace_memories(
        &self,
        drawer_prefix: Option<&str>,
    ) -> Result<Vec<String>> {
        match drawer_prefix {
            Some(prefix) => self.ids(
                "SELECT t.memory_id FROM mempalace_imports t
                   JOIN memories m ON m.id = t.memory_id
                  WHERE m.deleted_at IS NULL AND t.drawer_id LIKE ?",
                &[format!("{prefix}%")],
            ),
            None => self.ids(
                "SELECT t.memory_id FROM mempalace_imports t
                   JOIN memories m ON m.id = t.memory_id
                  WHERE m.deleted_at IS NULL",
                &[],
            ),
        }
    }

    /// Live memories that carry a mempalace source whether or not a
    /// tracking row records them, from drawers starting with
    /// `drawer_prefix`, or from any drawer when `None`.
    pub fn live_mempalace_shaped_memories(
        &self,
        drawer_prefix: Option<&str>,
    ) -> Result<Vec<String>> {
        match drawer_prefix {
            Some(prefix) => self.ids(
                "SELECT id FROM memories
                  WHERE deleted_at IS NULL
                    AND (source = 'mempalace_import' OR source LIKE 'mempalace:%')
                    AND json_extract(metadata, '$.mempalace_drawer_id') LIKE ?",
                &[format!("{prefix}%")],
            ),
            None => self.ids(
                "SELECT id FROM memories
                  WHERE deleted_at IS NULL
                    AND (source = 'mempalace_import' OR source LIKE 'mempalace:%')",
                &[],
            ),
        }
    }

    // --- undo ------------------------------------------------------------

    /// The distinct `doc_id`s of `memory_ids`.
    pub fn doc_ids_of(&self, memory_ids: &[String]) -> Result<Vec<String>> {
        if memory_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.ids(
            &format!(
                "SELECT DISTINCT doc_id FROM memories
                  WHERE doc_id IS NOT NULL AND id IN ({})",
                placeholders(memory_ids.len())
            ),
            memory_ids,
        )
    }

    /// Drop the dbs tracking rows for `memory_ids`. Returns how many went.
    pub fn forget_dbs(&self, memory_ids: &[String]) -> Result<usize> {
        self.forget_by_memory("dbs_imports", memory_ids)
    }

    /// Drop the mempalace tracking rows for `memory_ids`. Returns how many
    /// went.
    pub fn forget_mempalace(&self, memory_ids: &[String]) -> Result<usize> {
        self.forget_by_memory("mempalace_imports", memory_ids)
    }

    fn forget_by_memory(&self, table: &str, memory_ids: &[String]) -> Result<usize> {
        if memory_ids.is_empty() {
            return Ok(0);
        }
        self.conn.execute(
            &format!(
                "DELETE FROM {table} WHERE memory_id IN ({})",
                placeholders(memory_ids.len())
            ),
            params_from_iter(memory_ids.iter()),
        )
    }

    /// The first column of every row `sql` returns, bound to `bindings`.
    fn ids(&self, sql: &str, bindings: &[String]) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params_from_iter(bindings.iter()), |r| r.get(0))?
            .collect();
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    #[test]
    fn a_chat_import_is_forgotten_only_once_nothing_of_it_is_left() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let ledger = ImportLedger::new(&conn);
        ledger
            .record_chat("imp_a", "a.json", "ha", NOW, "{}")
            .unwrap();
        ledger
            .record_chat("imp_b", "b.json", "hb", NOW, "{}")
            .unwrap();
        Memories::new(&conn)
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
        let conn = db.conn();
        let ledger = ImportLedger::new(&conn);
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
        let conn = db.conn();
        let ledger = ImportLedger::new(&conn);
        ledger.record_mempalace("d1", "m1", NOW).unwrap();
        ledger.record_mempalace("d1", "m2", NOW).unwrap();
        assert_eq!(ledger.imported_drawers(&["d1", "d2"]).unwrap(), vec!["d1"]);
        assert_eq!(ledger.forget_mempalace(&["m1".to_string()]).unwrap(), 1);
    }
}
