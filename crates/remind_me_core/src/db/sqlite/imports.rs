//! Import tracking on SQLite: the [`ImportStore`] adapter for chat, dbs
//! and mempalace imports.

use super::memories::rows_where;
use super::{sqlite_error, SqliteTables};
use crate::db::engine::imports::extracted_text;
use crate::db::imports::{DbsKey, DbsTracked, ImportStore};
use crate::db::{Result, StoreError};
use rusqlite::params;
use std::collections::HashMap;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS chat_imports (
    import_id TEXT PRIMARY KEY, filename TEXT NOT NULL, hash TEXT NOT NULL,
    imported_at TEXT NOT NULL, stats TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS chat_imports_hash ON chat_imports (hash);
CREATE TABLE IF NOT EXISTS dbs_imports (
    dbs_source TEXT NOT NULL, external_id TEXT NOT NULL, memory_id TEXT NOT NULL,
    content_hash TEXT NOT NULL, imported_at TEXT NOT NULL,
    PRIMARY KEY (dbs_source, external_id));
CREATE INDEX IF NOT EXISTS dbs_imports_memory ON dbs_imports (memory_id);
CREATE TABLE IF NOT EXISTS mempalace_imports (
    drawer_id TEXT PRIMARY KEY, memory_id TEXT NOT NULL, imported_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS mempalace_imports_memory ON mempalace_imports (memory_id);
";

/// `items` as a JSON array, for `IN (SELECT value FROM json_each(?))`.
pub(super) fn json_list<S: AsRef<str>>(items: &[S]) -> String {
    serde_json::Value::from(items.iter().map(|s| s.as_ref()).collect::<Vec<_>>()).to_string()
}

/// Ids selected by `sql` with one bound value.
fn ids(t: &SqliteTables, sql: &str, value: Option<&str>) -> Result<Vec<String>> {
    t.all(sql, [value], |r| r.get(0))
}

impl ImportStore for SqliteTables {
    fn record_chat(
        &mut self,
        id: &str,
        filename: &str,
        hash: &str,
        at: &str,
        stats: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO chat_imports VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, filename, hash, at, stats],
            )
            .map(drop)
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _)
                    if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    StoreError::Engine(format!("sqlite: chat import {id:?} is already recorded"))
                }
                e => sqlite_error(e),
            })
    }

    fn chat_import_with_hash(&self, hash: &str) -> Result<Option<String>> {
        self.one(
            "SELECT import_id FROM chat_imports WHERE hash = ?1 ORDER BY imported_at, import_id LIMIT 1",
            [hash],
            |r| r.get(0),
        )
    }

    fn live_chat_memories(&self, import_id: Option<&str>) -> Result<Vec<String>> {
        ids(
            self,
            "SELECT id FROM memories WHERE deleted_at IS NULL AND (doc_id = ?1 OR \
             (?1 IS NULL AND doc_id IN (SELECT import_id FROM chat_imports))) ORDER BY id",
            import_id,
        )
    }

    fn chat_imports_with_nothing_left(&self, import_ids: &[String]) -> Result<Vec<String>> {
        self.all(
            "SELECT import_id FROM chat_imports WHERE import_id IN (SELECT value FROM json_each(?1)) \
             AND NOT EXISTS (SELECT 1 FROM memories m WHERE m.doc_id = import_id AND m.deleted_at IS NULL) \
             ORDER BY import_id",
            [json_list(import_ids)],
            |r| r.get(0),
        )
    }

    fn forget_chat_imports_with_nothing_left(&mut self, import_ids: &[String]) -> Result<usize> {
        let doomed = self.chat_imports_with_nothing_left(import_ids)?;
        self.exec(
            "DELETE FROM chat_imports WHERE import_id IN (SELECT value FROM json_each(?1))",
            [json_list(&doomed)],
        )
    }

    fn dbs_tracked(
        &self,
        source: &str,
        external_ids: &[&str],
    ) -> Result<HashMap<DbsKey, DbsTracked>> {
        let rows = self.all(
            "SELECT dbs_source, external_id, memory_id, content_hash FROM dbs_imports \
             WHERE dbs_source = ?1 AND external_id IN (SELECT value FROM json_each(?2))",
            params![source, json_list(external_ids)],
            |r| {
                Ok((
                    (r.get(0)?, r.get(1)?),
                    DbsTracked {
                        memory_id: r.get(2)?,
                        content_hash: r.get(3)?,
                    },
                ))
            },
        )?;
        Ok(rows.into_iter().collect())
    }

    fn record_dbs(
        &mut self,
        source: &str,
        external_id: &str,
        memory_id: &str,
        hash: &str,
        at: &str,
    ) -> Result<()> {
        self.exec(
            "INSERT OR REPLACE INTO dbs_imports VALUES (?1, ?2, ?3, ?4, ?5)",
            params![source, external_id, memory_id, hash, at],
        )
        .map(drop)
    }

    fn live_dbs_memories(&self, source_prefix: Option<&str>) -> Result<Vec<String>> {
        ids(
            self,
            "SELECT d.memory_id FROM dbs_imports d JOIN memories m ON m.id = d.memory_id \
             WHERE m.deleted_at IS NULL AND (?1 IS NULL OR d.dbs_source LIKE ?1 || '%') ORDER BY 1",
            source_prefix,
        )
    }

    fn imported_drawers(&self, drawer_ids: &[&str]) -> Result<Vec<String>> {
        self.all(
            "SELECT drawer_id FROM mempalace_imports WHERE drawer_id IN \
             (SELECT value FROM json_each(?1)) ORDER BY drawer_id",
            [json_list(drawer_ids)],
            |r| r.get(0),
        )
    }

    fn record_mempalace(&mut self, drawer_id: &str, memory_id: &str, at: &str) -> Result<()> {
        self.exec(
            "INSERT OR IGNORE INTO mempalace_imports VALUES (?1, ?2, ?3)",
            params![drawer_id, memory_id, at],
        )
        .map(drop)
    }

    fn live_tracked_mempalace_memories(&self, drawer_prefix: Option<&str>) -> Result<Vec<String>> {
        ids(
            self,
            "SELECT p.memory_id FROM mempalace_imports p JOIN memories m ON m.id = p.memory_id \
             WHERE m.deleted_at IS NULL AND (?1 IS NULL OR p.drawer_id LIKE ?1 || '%') ORDER BY 1",
            drawer_prefix,
        )
    }

    fn live_mempalace_shaped_memories(&self, drawer_prefix: Option<&str>) -> Result<Vec<String>> {
        let rows = rows_where(
            &self.conn,
            "deleted_at IS NULL AND (source = 'mempalace_import' OR source LIKE 'mempalace:%')",
            "id",
            &[],
        )?;
        let mut found = Vec::new();
        for row in rows {
            if let Some(prefix) = drawer_prefix {
                let Some(drawer) = extracted_text(&row.metadata, "mempalace_drawer_id")? else {
                    continue;
                };
                let like: bool = self
                    .one("SELECT ?1 LIKE ?2 || '%'", params![drawer, prefix], |r| {
                        r.get(0)
                    })?
                    .unwrap_or(false);
                if !like {
                    continue;
                }
            }
            found.push(row.id);
        }
        Ok(found)
    }

    fn doc_ids_of(&self, memory_ids: &[String]) -> Result<Vec<String>> {
        self.all(
            "SELECT DISTINCT doc_id FROM memories WHERE doc_id IS NOT NULL AND id IN \
             (SELECT value FROM json_each(?1)) ORDER BY doc_id",
            [json_list(memory_ids)],
            |r| r.get(0),
        )
    }

    fn forget_dbs(&mut self, memory_ids: &[String]) -> Result<usize> {
        self.exec(
            "DELETE FROM dbs_imports WHERE memory_id IN (SELECT value FROM json_each(?1))",
            [json_list(memory_ids)],
        )
    }

    fn forget_mempalace(&mut self, memory_ids: &[String]) -> Result<usize> {
        self.exec(
            "DELETE FROM mempalace_imports WHERE memory_id IN (SELECT value FROM json_each(?1))",
            [json_list(memory_ids)],
        )
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::imports::import_conformance!(SqliteTables::open_in_memory().unwrap());
}
