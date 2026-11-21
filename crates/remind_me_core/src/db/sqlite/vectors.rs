//! Embedding chunks and their metadata on SQLite: the [`VectorStore`]
//! adapter. Vectors are BLOBs, scored in process by the caller, as on the
//! engine.

use super::imports::json_list;
use super::memories::{rows_where, text};
use super::SqliteTables;
use crate::db::vectors::{ChunkVector, ConsolidationCandidate, Unembedded, VectorStore};
use crate::db::{Result, StoreError};
use rusqlite::{params, Row};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS vec_chunks (
    memory_id TEXT NOT NULL, chunk_ix INTEGER NOT NULL, embedding BLOB NOT NULL,
    PRIMARY KEY (memory_id, chunk_ix)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS embedding_meta (
    key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT NOT NULL);
";

fn vector(r: &Row<'_>) -> rusqlite::Result<ChunkVector> {
    Ok(ChunkVector {
        memory_id: r.get(0)?,
        embedding: r.get(1)?,
    })
}

fn count(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

impl VectorStore for SqliteTables {
    fn put(&mut self, memory_id: &str, chunk_ix: usize, embedding: &[u8]) -> Result<()> {
        let ix = i64::try_from(chunk_ix).unwrap_or(i64::MAX);
        self.exec(
            "INSERT OR REPLACE INTO vec_chunks VALUES (?1, ?2, ?3)",
            params![memory_id, ix, embedding],
        )
        .map(drop)
    }

    fn delete_for(&mut self, memory_id: &str) -> Result<usize> {
        self.exec("DELETE FROM vec_chunks WHERE memory_id = ?1", [memory_id])
    }

    fn clear(&mut self) -> Result<()> {
        self.exec("DELETE FROM vec_chunks", []).map(drop)
    }

    fn chunk_count(&self, memory_id: &str) -> Result<usize> {
        let n = self.one(
            "SELECT count(*) FROM vec_chunks WHERE memory_id = ?1",
            [memory_id],
            |r| r.get(0),
        )?;
        Ok(count(n.unwrap_or(0)))
    }

    fn chunks_for(&self, memory_id: &str) -> Result<Vec<ChunkVector>> {
        self.all(
            "SELECT memory_id, embedding FROM vec_chunks WHERE memory_id = ?1 ORDER BY chunk_ix",
            [memory_id],
            vector,
        )
    }

    fn count(&self) -> Result<usize> {
        Ok(count(
            self.one("SELECT count(*) FROM vec_chunks", [], |r| r.get(0))?
                .unwrap_or(0),
        ))
    }

    fn any_embedding(&self) -> Result<Option<Vec<u8>>> {
        self.one(
            "SELECT embedding FROM vec_chunks ORDER BY memory_id, chunk_ix LIMIT 1",
            [],
            |r| r.get(0),
        )
    }

    fn all(&self) -> Result<Vec<ChunkVector>> {
        SqliteTables::all(
            self,
            "SELECT memory_id, embedding FROM vec_chunks ORDER BY memory_id, chunk_ix",
            [],
            vector,
        )
    }

    fn live_chunks(
        &self,
        category: Option<&str>,
        among: Option<&[String]>,
    ) -> Result<Vec<ChunkVector>> {
        SqliteTables::all(
            self,
            "SELECT c.memory_id, c.embedding FROM vec_chunks c JOIN memories m ON m.id = c.memory_id \
             WHERE m.deleted_at IS NULL AND m.superseded_by IS NULL AND (?1 IS NULL OR m.category = ?1) \
             AND (?2 IS NULL OR c.memory_id IN (SELECT value FROM json_each(?2))) \
             ORDER BY c.memory_id, c.chunk_ix",
            params![category, among.map(json_list)],
            vector,
        )
    }

    fn unembedded(&self) -> Result<Vec<Unembedded>> {
        SqliteTables::all(
            self,
            "SELECT id, content FROM memories WHERE deleted_at IS NULL \
             AND id NOT IN (SELECT memory_id FROM vec_chunks) ORDER BY created_at, id",
            [],
            |r| {
                Ok(Unembedded {
                    id: r.get(0)?,
                    content: r.get(1)?,
                })
            },
        )
    }

    fn consolidation_candidates(
        &self,
        category: Option<&str>,
        n: usize,
    ) -> Result<Vec<ConsolidationCandidate>> {
        let mut bind = vec![text(crate::models::DIALOG_CATEGORY)];
        let mut clause = String::from(
            "status = 'active' AND deleted_at IS NULL AND superseded_by IS NULL AND category != ?1 \
             AND id IN (SELECT memory_id FROM vec_chunks WHERE chunk_ix = 0)",
        );
        if let Some(c) = category {
            bind.push(text(c));
            clause.push_str(" AND category = ?2");
        }
        let rows = rows_where(&self.conn, &clause, "created_at, id", &bind)?;
        let mut candidates = Vec::new();
        // Dialogs (D13) and project claims (ADR-0027) are never merged.
        for row in rows.into_iter().filter(|r| {
            !r.predicate
                .as_deref()
                .is_some_and(crate::project::is_reserved_predicate)
        }) {
            if candidates.len() >= n {
                break;
            }
            let embedding: Vec<u8> = self
                .one(
                    "SELECT embedding FROM vec_chunks WHERE memory_id = ?1 AND chunk_ix = 0",
                    [&row.id],
                    |r| r.get(0),
                )?
                .unwrap_or_default();
            let accessed_at = row.accessed_at.clone().ok_or_else(|| {
                StoreError::Engine(format!("sqlite: memory {:?} has no accessed_at", row.id))
            })?;
            candidates.push(ConsolidationCandidate {
                tags: serde_json::from_str(&row.tags).unwrap_or_default(),
                id: row.id,
                content: row.content,
                vitality: row.vitality,
                access_count: row.access_count,
                accessed_at,
                decay_rate: row.decay_rate,
                base_weight: row.base_weight,
                embedding,
            });
        }
        Ok(candidates)
    }

    fn meta(&self) -> Result<Vec<(String, String)>> {
        SqliteTables::all(
            self,
            "SELECT key, value FROM embedding_meta ORDER BY key, value",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    fn set_meta(&mut self, key: &str, value: &str, updated_at: &str) -> Result<()> {
        self.exec(
            "INSERT OR REPLACE INTO embedding_meta VALUES (?1, ?2, ?3)",
            [key, value, updated_at],
        )
        .map(drop)
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::vectors::vector_conformance!(SqliteTables::open_in_memory().unwrap());
}
