//! Storage for search expansion: `memory_associations` (how often two
//! memories were retrieved together) and the reads that find a result's
//! relatives through shared entities, document position and co-retrieval.
//!
//! ADR-0023 phase 1, step 8. Every statement [`crate::expansion`] ran lives
//! here. The rules stay there: the pair cap, the weight ceiling, the window
//! size, how relatives are grouped and capped.

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, Result};

/// A live memory another one points at, with the fields expansion shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Relative {
    pub id: String,
    pub content: String,
    pub category: String,
    pub created_at: String,
}

/// A relative found through an entity, one row per entity it shares.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityRelative {
    pub memory: Relative,
    pub entity_name: String,
}

/// A chunk of a document, with its position.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentChunk {
    pub memory: Relative,
    pub doc_id: Option<String>,
    pub chunk_index: Option<i64>,
}

/// A relative found through co-retrieval, with the pair's weight.
#[derive(Debug, Clone, PartialEq)]
pub struct CoRetrieved {
    pub memory: Relative,
    pub weight: i64,
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// `ids` twice over, for a query that binds the list on both sides.
fn twice(ids: &[String]) -> Vec<SqlValue> {
    ids.iter()
        .chain(ids.iter())
        .map(|id| SqlValue::Text(id.clone()))
        .collect()
}

/// The expansion tables and reads, over one connection.
pub struct Related<'c> {
    conn: &'c Connection,
}

impl<'c> Related<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Record that `a` and `b` were retrieved together at `now`: a new pair
    /// starts at weight 1, and a known one gains 1 up to `max_weight`. The
    /// caller orders the pair, so `(a, b)` and `(b, a)` are one row.
    pub fn bump_pair(&self, a: &str, b: &str, now: &str, max_weight: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO memory_associations (memory_id_a, memory_id_b, weight, updated_at)
             VALUES (?, ?, 1, ?)
             ON CONFLICT(memory_id_a, memory_id_b) DO UPDATE SET
                 weight = MIN(weight + 1, ?),
                 updated_at = excluded.updated_at",
            params![a, b, now, max_weight],
        )?;
        Ok(())
    }

    /// Live memories outside `seed_ids` that mention an entity a seed
    /// mentions, newest first, one row per shared entity. A link whose
    /// memory or entity is not stored here is skipped.
    pub fn via_entities(&self, seed_ids: &[String]) -> Result<Vec<EntityRelative>> {
        if seed_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ph = placeholders(seed_ids.len());
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id, m.content, m.category, m.created_at, e.name AS entity_name
               FROM memory_entities seed
               JOIN memory_entities nbr ON nbr.entity_id = seed.entity_id
               JOIN entities e ON e.id = seed.entity_id
               JOIN memories m ON m.id = nbr.memory_id
              WHERE seed.memory_id IN ({ph})
                AND nbr.memory_id NOT IN ({ph})
                AND m.superseded_by IS NULL
                AND m.deleted_at IS NULL
              ORDER BY m.created_at DESC, m.id, e.name"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(twice(seed_ids)), |row| {
                Ok(EntityRelative {
                    memory: Relative {
                        id: row.get("id")?,
                        content: row.get("content")?,
                        category: row.get("category")?,
                        created_at: row.get("created_at")?,
                    },
                    entity_name: row.get("entity_name")?,
                })
            })?
            .collect();
        rows
    }

    /// Live chunks of `doc_id` from position `from` to `to`, in order.
    pub fn document_window(&self, doc_id: &str, from: i64, to: i64) -> Result<Vec<DocumentChunk>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, category, created_at, doc_id, chunk_index
               FROM memories
              WHERE doc_id = ?
                AND chunk_index BETWEEN ? AND ?
                AND superseded_by IS NULL
                AND deleted_at IS NULL
              ORDER BY chunk_index",
        )?;
        let rows = stmt
            .query_map(params![doc_id, from, to], |row| {
                Ok(DocumentChunk {
                    memory: Relative {
                        id: row.get("id")?,
                        content: row.get("content")?,
                        category: row.get("category")?,
                        created_at: row.get("created_at")?,
                    },
                    doc_id: row.get("doc_id")?,
                    chunk_index: row.get("chunk_index")?,
                })
            })?
            .collect();
        rows
    }

    /// Live memories paired with any of `seed_ids`, strongest pair first,
    /// then newest. A pair is read from either side, since it is stored once.
    pub fn co_retrieved(&self, seed_ids: &[String]) -> Result<Vec<CoRetrieved>> {
        if seed_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ph = placeholders(seed_ids.len());
        let mut stmt = self.conn.prepare(&format!(
            "SELECT assoc.other_id, assoc.weight, m.content, m.category, m.created_at
               FROM (
                    SELECT memory_id_b AS other_id, weight FROM memory_associations
                     WHERE memory_id_a IN ({ph})
                    UNION ALL
                    SELECT memory_id_a AS other_id, weight FROM memory_associations
                     WHERE memory_id_b IN ({ph})
               ) assoc
               JOIN memories m ON m.id = assoc.other_id
              WHERE m.superseded_by IS NULL AND m.deleted_at IS NULL
              ORDER BY assoc.weight DESC, m.created_at DESC"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(twice(seed_ids)), |row| {
                Ok(CoRetrieved {
                    memory: Relative {
                        id: row.get("other_id")?,
                        content: row.get("content")?,
                        category: row.get("category")?,
                        created_at: row.get("created_at")?,
                    },
                    weight: row.get("weight")?,
                })
            })?
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
    fn a_pair_is_read_from_either_side_and_its_weight_is_capped() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let memories = Memories::new(&conn);
        for id in ["a", "b"] {
            memories.insert(&NewMemory::new(id, id, NOW)).unwrap();
        }
        let related = Related::new(&conn);
        for _ in 0..5 {
            related.bump_pair("a", "b", NOW, 3).unwrap();
        }

        let from_a = related.co_retrieved(&["a".to_string()]).unwrap();
        let from_b = related.co_retrieved(&["b".to_string()]).unwrap();
        assert_eq!((from_a[0].memory.id.as_str(), from_a[0].weight), ("b", 3));
        assert_eq!(from_b[0].memory.id, "a");
        assert!(related.co_retrieved(&[]).unwrap().is_empty());
    }
}
