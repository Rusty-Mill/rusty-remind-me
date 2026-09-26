//! Storage reads for the curation queues: captures awaiting decomposition,
//! raw imports awaiting normalization, contradiction candidates, and the
//! maintenance counts over all of them.
//!
//! ADR-0023 phase 1, step 5c. Every statement [`crate::capture`],
//! [`crate::normalize`], [`crate::maintenance`] and
//! [`crate::contradictions`] ran lives here. The rules stay there: snippet
//! lengths, batch bounds, which sources count as raw imports, the entity
//! fan-out ceiling, and the keyset cursor's meaning.

use crate::db::queries::{parse_memory_row, MEMORY_COLUMNS};
use crate::models::{ContradictionSide, Memory};
use rusqlite::{params, Connection, OptionalExtension, Result};

/// A capture that nothing has been decomposed from yet.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureRow {
    pub id: String,
    pub capture_id: String,
    pub content: String,
    pub category: String,
    pub tags: Vec<String>,
}

/// A raw import that nothing has been normalized from yet.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportRow {
    pub id: String,
    pub content: String,
    pub category: String,
    pub source: String,
    pub tags: Vec<String>,
    pub metadata: serde_json::Value,
}

/// What a normalization copies from the memory it distils.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizationSource {
    pub tags: Vec<String>,
    pub doc_id: Option<String>,
    pub chunk_index: Option<i64>,
}

/// One maintenance backlog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backlog {
    /// Captures with no decomposed facts pointing back at them.
    Undecomposed,
    /// Live memories, other than raw dialogs, with neither a triple nor an
    /// entity mention.
    Unannotated,
    /// Live raw imports nothing names as `normalized_from`.
    Unnormalized,
    /// Live memories whose `memory_type` is still `unclassified`.
    Unclassified,
}

/// How many distinct captures there are, and when the newest was written.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureActivity {
    pub captures: i64,
    pub last_capture_at: Option<String>,
}

/// A capture no fact names as its source.
const UNDECOMPOSED_CAPTURE: &str = "m.capture_id IS NOT NULL
         AND m.source_capture_id IS NULL
         AND m.deleted_at IS NULL
         AND NOT EXISTS (
             SELECT 1 FROM memories c WHERE c.source_capture_id = m.capture_id
         )";

/// The live raw imports from `sources` no memory names as its
/// `normalized_from`.
fn unnormalized_where(sources: &[&str]) -> String {
    let sources = sources
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "m.superseded_by IS NULL
         AND m.deleted_at IS NULL
         AND m.source IN ({sources})
         AND NOT EXISTS (
             SELECT 1 FROM memories n
              WHERE json_extract(n.metadata, '$.normalized_from') = m.id
         )"
    )
}

/// Every pair of live, non-dialog memories sharing an entity mentioned at
/// most `max_fanout` times, excluding pairs with the same subject and
/// predicate. Columns `id_a`, `id_b`, with `id_a < id_b`.
///
/// The triple exclusion is subtler than it looks. A pair where both sides
/// share a normalised (subject, predicate) but differ in object cannot be
/// observed here: the moment the second was written, the supersession check
/// set `superseded_by` on the first, and this query only considers live
/// rows. So the exclusion filters out same-object restatements, not pairs
/// that could otherwise slip through.
///
/// `lower`/`trim` approximates the entity-name normalisation rather than
/// reproducing it exactly. This only narrows a set for review, so an
/// imprecise exclusion is a false negative, not a correctness bug.
fn contradiction_pairs(max_fanout: i64) -> String {
    format!(
        "SELECT DISTINCT me1.memory_id AS id_a, me2.memory_id AS id_b
           FROM memory_entities me1
           JOIN memory_entities me2
             ON me2.entity_id = me1.entity_id AND me2.memory_id > me1.memory_id
           JOIN memories m1 ON m1.id = me1.memory_id
           JOIN memories m2 ON m2.id = me2.memory_id
           JOIN (
               SELECT entity_id, COUNT(*) AS mentions
                 FROM memory_entities
                GROUP BY entity_id
           ) fanout ON fanout.entity_id = me1.entity_id
          WHERE m1.superseded_by IS NULL AND m1.deleted_at IS NULL
            AND m2.superseded_by IS NULL AND m2.deleted_at IS NULL
            AND m1.category != 'dialog' AND m2.category != 'dialog'
            AND fanout.mentions <= {max_fanout}
            AND NOT (
                m1.subject IS NOT NULL AND m1.predicate IS NOT NULL
                AND m2.subject IS NOT NULL AND m2.predicate IS NOT NULL
                AND lower(trim(m1.subject)) = lower(trim(m2.subject))
                AND lower(trim(m1.predicate)) = lower(trim(m2.predicate))
            )"
    )
}

/// The curation reads, over one connection.
pub struct Curation<'c> {
    conn: &'c Connection,
}

impl<'c> Curation<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    fn count(&self, sql: &str) -> Result<i64> {
        self.conn.query_row(sql, [], |r| r.get(0))
    }

    // --- captures --------------------------------------------------------

    /// Every memory carrying `capture_id`, by category.
    pub fn capture_rows(&self, capture_id: &str) -> Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {MEMORY_COLUMNS} FROM memories WHERE capture_id = ? ORDER BY category"
        ))?;
        let rows = stmt
            .query_map(params![capture_id], parse_memory_row)?
            .collect();
        rows
    }

    /// The tags of one memory carrying `capture_id`, or `None` when none
    /// does. Unparseable tags read as none.
    pub fn capture_tags(&self, capture_id: &str) -> Result<Option<Vec<String>>> {
        self.conn
            .query_row(
                "SELECT tags FROM memories WHERE capture_id = ? LIMIT 1",
                params![capture_id],
                |row| {
                    let tags_json: String = row.get(0)?;
                    Ok(serde_json::from_str(&tags_json).unwrap_or_default())
                },
            )
            .optional()
    }

    /// Captures nothing has been decomposed from, newest first, at most
    /// `limit`.
    pub fn undecomposed(&self, limit: usize) -> Result<Vec<CaptureRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id, m.capture_id, m.content, m.category, m.tags
               FROM memories m
              WHERE {UNDECOMPOSED_CAPTURE}
              ORDER BY m.created_at DESC
              LIMIT ?"
        ))?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let tags_json: String = row.get("tags")?;
                Ok(CaptureRow {
                    id: row.get("id")?,
                    capture_id: row.get("capture_id")?,
                    content: row.get("content")?,
                    category: row.get("category")?,
                    tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                })
            })?
            .collect();
        rows
    }

    /// How many captures nothing has been decomposed from.
    pub fn count_undecomposed(&self) -> Result<i64> {
        self.count(&format!(
            "SELECT count(*) FROM memories m WHERE {UNDECOMPOSED_CAPTURE}"
        ))
    }

    /// How many distinct live captures there are, and the newest one's
    /// `created_at`.
    pub fn capture_activity(&self) -> Result<CaptureActivity> {
        self.conn.query_row(
            "SELECT COUNT(DISTINCT capture_id), MAX(created_at) FROM memories
              WHERE capture_id IS NOT NULL AND deleted_at IS NULL",
            [],
            |r| {
                Ok(CaptureActivity {
                    captures: r.get(0)?,
                    last_capture_at: r.get(1)?,
                })
            },
        )
    }

    // --- normalization ---------------------------------------------------

    /// Live raw imports from `sources` nothing has been normalized from,
    /// newest first, at most `limit`. Unparseable metadata reads as `{}`.
    pub fn unnormalized(&self, sources: &[&str], limit: usize) -> Result<Vec<ImportRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id, m.content, m.category, m.source, m.tags, m.metadata
               FROM memories m
              WHERE {}
              ORDER BY m.created_at DESC
              LIMIT ?",
            unnormalized_where(sources)
        ))?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let tags_json: String = row.get("tags")?;
                let metadata_json: String = row.get("metadata")?;
                Ok(ImportRow {
                    id: row.get("id")?,
                    content: row.get("content")?,
                    category: row.get("category")?,
                    source: row.get("source")?,
                    tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                    metadata: serde_json::from_str(&metadata_json)
                        .unwrap_or_else(|_| serde_json::json!({})),
                })
            })?
            .collect();
        rows
    }

    /// How many live raw imports from `sources` nothing has been
    /// normalized from.
    pub fn count_unnormalized(&self, sources: &[&str]) -> Result<i64> {
        self.count(&format!(
            "SELECT count(*) FROM memories m WHERE {}",
            unnormalized_where(sources)
        ))
    }

    /// What a normalization of `memory_id` copies from it, if it exists.
    pub fn normalization_source(&self, memory_id: &str) -> Result<Option<NormalizationSource>> {
        self.conn
            .query_row(
                "SELECT tags, doc_id, chunk_index FROM memories WHERE id = ?",
                params![memory_id],
                |r| {
                    let tags_json: String = r.get(0)?;
                    Ok(NormalizationSource {
                        tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                        doc_id: r.get(1)?,
                        chunk_index: r.get(2)?,
                    })
                },
            )
            .optional()
    }

    // --- maintenance -----------------------------------------------------

    /// How deep `backlog` is.
    pub fn backlog_depth(&self, backlog: Backlog) -> Result<i64> {
        self.count(match backlog {
            Backlog::Undecomposed => {
                "SELECT COUNT(*) FROM memories m
                  WHERE m.capture_id IS NOT NULL
                    AND m.source_capture_id IS NULL
                    AND m.deleted_at IS NULL
                    AND NOT EXISTS (
                        SELECT 1 FROM memories c WHERE c.source_capture_id = m.capture_id
                    )"
            }
            Backlog::Unannotated => {
                "SELECT COUNT(*) FROM memories m
                  WHERE m.superseded_by IS NULL
                    AND m.deleted_at IS NULL
                    AND m.category != 'dialog'
                    AND m.subject IS NULL AND m.predicate IS NULL AND m.object IS NULL
                    AND NOT EXISTS (
                        SELECT 1 FROM memory_entities me WHERE me.memory_id = m.id
                    )"
            }
            // `NOT IN` over an uncorrelated subquery rather than `NOT EXISTS`:
            // correlated, SQLite re-scans the index once per candidate row,
            // which on a large vault is a per-row scan rather than a seek. The
            // set form materialises once and probes per row.
            Backlog::Unnormalized => {
                "SELECT COUNT(*) FROM memories m
                  WHERE m.superseded_by IS NULL
                    AND m.deleted_at IS NULL
                    AND m.source IN ('document_import', 'chat_import')
                    AND m.id NOT IN (
                        SELECT json_extract(metadata, '$.normalized_from') FROM memories
                        WHERE json_extract(metadata, '$.normalized_from') IS NOT NULL
                    )"
            }
            Backlog::Unclassified => {
                "SELECT COUNT(*) FROM memories m
                  WHERE m.memory_type = 'unclassified' AND m.deleted_at IS NULL"
            }
        })
    }

    // --- contradictions --------------------------------------------------

    /// How many contradiction candidate pairs there are.
    pub fn count_contradiction_pairs(&self, max_fanout: i64) -> Result<i64> {
        self.count(&format!(
            "SELECT COUNT(*) FROM ({}) p",
            contradiction_pairs(max_fanout)
        ))
    }

    /// Contradiction candidate pairs in `(id_a, id_b)` order, after `cursor`
    /// when given, at most `limit`.
    pub fn contradiction_pairs(
        &self,
        max_fanout: i64,
        cursor: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let cursor_clause = if cursor.is_some() {
            "WHERE (id_a > ?1 OR (id_a = ?1 AND id_b > ?2))"
        } else {
            ""
        };
        let sql = format!(
            "SELECT id_a, id_b FROM ({}) {cursor_clause} ORDER BY id_a, id_b LIMIT ?3",
            contradiction_pairs(max_fanout)
        );
        // Without a cursor the slots it would fill are bound to NULL: the
        // clause naming them is not in the SQL, so they are never read.
        let (after_a, after_b) = match cursor {
            Some((a, b)) => (Some(a), Some(b)),
            None => (None, None),
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![after_a, after_b, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect();
        rows
    }

    /// The names of the entities both memories mention, by name.
    pub fn shared_entity_names(&self, id_a: &str, id_b: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT e.name FROM memory_entities me1
               JOIN memory_entities me2 ON me2.entity_id = me1.entity_id
               JOIN entities e ON e.id = me1.entity_id
              WHERE me1.memory_id = ? AND me2.memory_id = ?
              ORDER BY e.name",
        )?;
        let rows = stmt.query_map(params![id_a, id_b], |r| r.get(0))?.collect();
        rows
    }

    /// One side of a contradiction pair, with the first `snippet_chars`
    /// characters of its content.
    pub fn contradiction_side(
        &self,
        memory_id: &str,
        snippet_chars: usize,
    ) -> Result<ContradictionSide> {
        self.conn.query_row(
            &format!(
                "SELECT id, substr(content, 1, {snippet_chars}) AS content_snippet, category,
                        memory_type, subject, predicate, object, created_at
                   FROM memories WHERE id = ?"
            ),
            params![memory_id],
            |r| {
                Ok(ContradictionSide {
                    id: r.get(0)?,
                    content_snippet: r.get(1)?,
                    category: r.get(2)?,
                    memory_type: r.get(3)?,
                    subject: r.get(4)?,
                    predicate: r.get(5)?,
                    object: r.get(6)?,
                    created_at: r.get(7)?,
                })
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    #[test]
    fn a_capture_leaves_the_backlog_once_a_fact_names_it() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let memories = Memories::new(&conn);
        memories
            .insert(&NewMemory {
                capture_id: Some("cap".to_string()),
                ..NewMemory::new("dialog", "x", NOW)
            })
            .unwrap();
        let curation = Curation::new(&conn);
        assert_eq!(curation.count_undecomposed().unwrap(), 1);
        assert_eq!(curation.backlog_depth(Backlog::Undecomposed).unwrap(), 1);

        memories
            .insert(&NewMemory {
                source_capture_id: Some("cap".to_string()),
                ..NewMemory::new("fact", "y", NOW)
            })
            .unwrap();
        assert_eq!(curation.count_undecomposed().unwrap(), 0);
        assert!(curation.undecomposed(10).unwrap().is_empty());
        assert_eq!(curation.backlog_depth(Backlog::Undecomposed).unwrap(), 0);
    }

    #[test]
    fn an_import_leaves_the_backlog_once_normalized() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let memories = Memories::new(&conn);
        memories
            .insert(&NewMemory {
                source: "chat_import".to_string(),
                ..NewMemory::new("raw", "x", NOW)
            })
            .unwrap();
        let curation = Curation::new(&conn);
        let sources = ["document_import", "chat_import"];
        assert_eq!(curation.count_unnormalized(&sources).unwrap(), 1);
        assert_eq!(curation.backlog_depth(Backlog::Unnormalized).unwrap(), 1);

        memories
            .insert(&NewMemory {
                metadata: serde_json::json!({ "normalized_from": "raw" }),
                ..NewMemory::new("norm", "y", NOW)
            })
            .unwrap();
        assert_eq!(curation.count_unnormalized(&sources).unwrap(), 0);
        assert_eq!(curation.backlog_depth(Backlog::Unnormalized).unwrap(), 0);
    }

    #[test]
    fn contradiction_pairs_page_by_keyset() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let memories = Memories::new(&conn);
        let entities = crate::db::entities::Entities::new(&conn);
        for id in ["a", "b", "c"] {
            memories.insert(&NewMemory::new(id, id, NOW)).unwrap();
            entities
                .link(id, "e", NOW, crate::db::derived::Origin::Local)
                .unwrap();
        }
        let curation = Curation::new(&conn);
        assert_eq!(curation.count_contradiction_pairs(20).unwrap(), 3);

        let first = curation.contradiction_pairs(20, None, 2).unwrap();
        assert_eq!(
            first,
            vec![
                ("a".to_string(), "b".to_string()),
                ("a".to_string(), "c".to_string())
            ]
        );
        let rest = curation
            .contradiction_pairs(20, Some(("a", "c")), 2)
            .unwrap();
        assert_eq!(rest, vec![("b".to_string(), "c".to_string())]);
        assert_eq!(curation.count_contradiction_pairs(2).unwrap(), 0);
    }
}
