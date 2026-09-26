//! Storage for embeddings: `vec_chunks`, one row per chunk of a memory,
//! keyed `(memory_id, chunk_ix)` and holding the vector as little-endian
//! f32 bytes, plus `embedding_meta`, which records the model that wrote
//! them.
//!
//! ADR-0023 phase 1, step 4. Chunks were keyed on `memories.rowid` up to
//! schema v29; they are keyed on the memory id now, so nothing depends on
//! a row number another store would not have. Every statement
//! [`crate::vectors`], [`crate::ann_index`] and [`crate::consolidation`] ran
//! against these tables lives here. The rules stay there: chunking,
//! scoring, the ANN index's staleness test, and the model-change clear.

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Result};

/// One stored chunk vector.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkVector {
    pub memory_id: String,
    pub embedding: Vec<u8>,
}

/// A memory with no chunk vectors.
#[derive(Debug, Clone, PartialEq)]
pub struct Unembedded {
    pub id: String,
    pub content: String,
}

/// A consolidation candidate: an active memory and its first chunk vector.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationCandidate {
    pub id: String,
    pub content: String,
    pub vitality: f64,
    pub access_count: i64,
    pub accessed_at: String,
    pub tags: Vec<String>,
    pub decay_rate: f64,
    pub base_weight: f64,
    pub embedding: Vec<u8>,
}

/// One placeholder per item, comma-separated.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// The vector tables, over one connection.
pub struct Vectors<'c> {
    conn: &'c Connection,
}

impl<'c> Vectors<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Store `embedding` as chunk `chunk_ix` of `memory_id`, replacing any
    /// chunk already there.
    pub fn put(&self, memory_id: &str, chunk_ix: usize, embedding: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO vec_chunks (memory_id, chunk_ix, embedding) VALUES (?, ?, ?)",
            params![memory_id, chunk_ix as i64, embedding],
        )?;
        Ok(())
    }

    /// Delete every chunk of `memory_id`. Returns how many went.
    pub fn delete_for(&self, memory_id: &str) -> Result<usize> {
        self.conn.execute(
            "DELETE FROM vec_chunks WHERE memory_id = ?",
            params![memory_id],
        )
    }

    /// Delete every chunk of every memory.
    pub fn clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM vec_chunks", [])?;
        Ok(())
    }

    /// How many chunks `memory_id` has.
    pub fn chunk_count(&self, memory_id: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM vec_chunks WHERE memory_id = ?",
            params![memory_id],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// How many chunks are stored in all.
    pub fn count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM vec_chunks", [], |r| r.get(0))?;
        Ok(count.max(0) as usize)
    }

    /// Any one stored embedding, if there is one.
    pub fn any_embedding(&self) -> Result<Option<Vec<u8>>> {
        self.conn
            .query_row("SELECT embedding FROM vec_chunks LIMIT 1", [], |r| r.get(0))
            .optional()
    }

    /// Every stored chunk, in key order.
    pub fn all(&self) -> Result<Vec<ChunkVector>> {
        let mut stmt = self
            .conn
            .prepare("SELECT memory_id, embedding FROM vec_chunks ORDER BY memory_id, chunk_ix")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ChunkVector {
                    memory_id: r.get(0)?,
                    embedding: r.get(1)?,
                })
            })?
            .collect();
        rows
    }

    /// The chunks of live, unsuperseded memories, of `category` when given,
    /// and only of the memories in `among` when given.
    pub fn live_chunks(
        &self,
        category: Option<&str>,
        among: Option<&[String]>,
    ) -> Result<Vec<ChunkVector>> {
        let mut sql = String::from(
            "SELECT vc.memory_id, vc.embedding
               FROM vec_chunks vc
               JOIN memories m ON m.id = vc.memory_id
              WHERE m.superseded_by IS NULL AND m.deleted_at IS NULL",
        );
        let mut bindings: Vec<SqlValue> = Vec::new();
        if let Some(category) = category {
            sql.push_str(" AND m.category = ?");
            bindings.push(SqlValue::Text(category.to_string()));
        }
        if let Some(ids) = among {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            sql.push_str(&format!(
                " AND vc.memory_id IN ({})",
                placeholders(ids.len())
            ));
            bindings.extend(ids.iter().map(|id| SqlValue::Text(id.clone())));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(bindings), |r| {
                Ok(ChunkVector {
                    memory_id: r.get(0)?,
                    embedding: r.get(1)?,
                })
            })?
            .collect();
        rows
    }

    /// Every live memory with no chunk vectors, oldest first.
    pub fn unembedded(&self) -> Result<Vec<Unembedded>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.content
               FROM memories m
              WHERE m.deleted_at IS NULL
                AND NOT EXISTS (SELECT 1 FROM vec_chunks vc WHERE vc.memory_id = m.id)
              ORDER BY m.created_at, m.id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Unembedded {
                    id: row.get(0)?,
                    content: row.get(1)?,
                })
            })?
            .collect();
        rows
    }

    /// Active, unsuperseded, live memories with a first chunk vector, of
    /// `category` when given, at most `limit`.
    pub fn consolidation_candidates(
        &self,
        category: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConsolidationCandidate>> {
        let mut sql = String::from(
            "SELECT m.id, m.content, m.vitality, m.access_count, m.accessed_at, m.tags,
                    m.decay_rate, m.base_weight, vc.embedding
               FROM memories m
               JOIN vec_chunks vc ON vc.memory_id = m.id AND vc.chunk_ix = 0
              WHERE m.status = 'active' AND m.superseded_by IS NULL AND m.deleted_at IS NULL",
        );
        let mut bindings: Vec<SqlValue> = Vec::new();
        if let Some(category) = category {
            sql.push_str(" AND m.category = ?");
            bindings.push(SqlValue::Text(category.to_string()));
        }
        sql.push_str(" LIMIT ?");
        bindings.push(SqlValue::Integer(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(bindings), |row| {
                let tags_json: String = row.get(5)?;
                Ok(ConsolidationCandidate {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    vitality: row.get(2)?,
                    access_count: row.get(3)?,
                    accessed_at: row.get(4)?,
                    tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                    decay_rate: row.get(6)?,
                    base_weight: row.get(7)?,
                    embedding: row.get(8)?,
                })
            })?
            .collect();
        rows
    }

    // --- embedding_meta --------------------------------------------------

    /// Every `embedding_meta` key and value.
    pub fn meta(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM embedding_meta")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect();
        rows
    }

    /// Set the `embedding_meta` value under `key`, stamped `updated_at`.
    pub fn set_meta(&self, key: &str, value: &str, updated_at: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO embedding_meta (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, updated_at],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    #[test]
    fn live_chunks_skip_deleted_memories_and_respect_the_narrowing() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let memories = Memories::new(&conn);
        memories.insert(&NewMemory::new("a", "x", NOW)).unwrap();
        memories
            .insert(&NewMemory {
                deleted_at: Some(NOW.to_string()),
                ..NewMemory::new("gone", "x", NOW)
            })
            .unwrap();
        let vectors = Vectors::new(&conn);
        vectors.put("a", 0, &[1, 0, 0, 0]).unwrap();
        vectors.put("a", 1, &[2, 0, 0, 0]).unwrap();
        vectors.put("gone", 0, &[3, 0, 0, 0]).unwrap();

        assert_eq!(vectors.live_chunks(None, None).unwrap().len(), 2);
        assert!(vectors
            .live_chunks(None, Some(&["other".to_string()]))
            .unwrap()
            .is_empty());
        assert!(vectors.live_chunks(None, Some(&[])).unwrap().is_empty());
        assert_eq!(vectors.count().unwrap(), 3);
        assert_eq!(vectors.delete_for("a").unwrap(), 2);
        assert_eq!(vectors.chunk_count("a").unwrap(), 0);
    }

    #[test]
    fn a_memory_with_a_chunk_is_not_unembedded() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let memories = Memories::new(&conn);
        memories.insert(&NewMemory::new("a", "x", NOW)).unwrap();
        memories.insert(&NewMemory::new("b", "y", NOW)).unwrap();
        let vectors = Vectors::new(&conn);
        vectors.put("a", 0, &[1, 0, 0, 0]).unwrap();

        let missing: Vec<String> = vectors
            .unembedded()
            .unwrap()
            .into_iter()
            .map(|u| u.id)
            .collect();
        assert_eq!(missing, vec!["b"]);
    }
}
