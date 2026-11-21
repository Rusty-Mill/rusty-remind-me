//! Related-memory reads and co-retrieval pairs on SQLite: the
//! [`RelatedStore`] adapter.

use super::imports::json_list;
use super::memories::{memory_row, rows_where, text};
use super::{sqlite_error, SqliteTables};
use crate::db::engine::related::relative;
use crate::db::related::{CoRetrieved, DocumentChunk, EntityRelative, RelatedStore};
use crate::db::Result;
use crate::testing::MEMORY_COLUMNS;
use rusqlite::params;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memory_associations (
    memory_id_a TEXT NOT NULL, memory_id_b TEXT NOT NULL, weight INTEGER NOT NULL,
    updated_at TEXT NOT NULL, PRIMARY KEY (memory_id_a, memory_id_b)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS memory_associations_b ON memory_associations (memory_id_b);
";

/// Every memory column, qualified as `m.<column>`.
fn m_columns() -> String {
    MEMORY_COLUMNS
        .iter()
        .map(|c| format!("m.{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

const LIVE: &str = "m.deleted_at IS NULL AND m.superseded_by IS NULL";

impl RelatedStore for SqliteTables {
    fn bump_pair(&mut self, a: &str, b: &str, now: &str, max_weight: i64) -> Result<()> {
        self.exec(
            "INSERT INTO memory_associations VALUES (?1, ?2, 1, ?3) ON CONFLICT DO UPDATE \
             SET weight = min(weight + 1, ?4), updated_at = ?3",
            params![a, b, now, max_weight],
        )
        .map(drop)
    }

    fn via_entities(&self, seed_ids: &[String]) -> Result<Vec<EntityRelative>> {
        let sql = format!(
            "SELECT {}, e.name FROM memory_entities s JOIN entities e ON e.id = s.entity_id \
             JOIN memory_entities n ON n.entity_id = s.entity_id JOIN memories m ON m.id = n.memory_id \
             WHERE s.memory_id IN (SELECT value FROM json_each(?1)) \
             AND n.memory_id NOT IN (SELECT value FROM json_each(?1)) AND {LIVE} \
             ORDER BY m.created_at DESC, m.id, e.name",
            m_columns()
        );
        self.all(&sql, [json_list(seed_ids)], |r| {
            Ok(EntityRelative {
                memory: relative(&memory_row(r)?),
                entity_name: r.get(MEMORY_COLUMNS.len())?,
            })
        })
    }

    fn unlink_memory(&mut self, memory_id: &str) -> Result<()> {
        self.exec(
            "DELETE FROM memory_associations WHERE memory_id_a = ?1 OR memory_id_b = ?1",
            [memory_id],
        )
        .map(drop)
    }

    fn document_window(&self, doc_id: &str, from: i64, to: i64) -> Result<Vec<DocumentChunk>> {
        let rows = rows_where(
            &self.conn,
            "deleted_at IS NULL AND superseded_by IS NULL AND doc_id = ?1 AND chunk_index BETWEEN ?2 AND ?3",
            "chunk_index, id",
            &[text(doc_id), from.into(), to.into()],
        )?;
        Ok(rows
            .iter()
            .map(|row| DocumentChunk {
                memory: relative(row),
                doc_id: row.doc_id.clone(),
                chunk_index: row.chunk_index,
            })
            .collect())
    }

    fn co_retrieved(&self, seed_ids: &[String]) -> Result<Vec<CoRetrieved>> {
        let sql = format!(
            "SELECT {cols}, p.weight FROM (\
               SELECT memory_id_b AS other, weight FROM memory_associations \
                 WHERE memory_id_a IN (SELECT value FROM json_each(?1)) \
               UNION ALL SELECT memory_id_a, weight FROM memory_associations \
                 WHERE memory_id_b IN (SELECT value FROM json_each(?1))) p \
             JOIN memories m ON m.id = p.other WHERE {LIVE} \
             ORDER BY p.weight DESC, m.created_at DESC, m.id",
            cols = m_columns()
        );
        let mut stmt = self.conn.prepare_cached(&sql).map_err(sqlite_error)?;
        let rows = stmt
            .query_map([json_list(seed_ids)], |r| {
                Ok(CoRetrieved {
                    memory: relative(&memory_row(r)?),
                    weight: r.get(MEMORY_COLUMNS.len())?,
                })
            })
            .map_err(sqlite_error)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sqlite_error)
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::related::related_conformance!(SqliteTables::open_in_memory().unwrap());
}
