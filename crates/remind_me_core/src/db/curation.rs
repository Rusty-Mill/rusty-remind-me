//! Storage reads for the curation queues: captures awaiting decomposition,
//! raw imports awaiting normalization, contradiction candidates, and the
//! maintenance counts over all of them.
//!
//! ADR-0023 phase 1, step 5c. Every statement [`crate::capture`],
//! [`crate::normalize`], [`crate::maintenance`] and
//! [`crate::contradictions`] ran lives here. The rules stay there: snippet
//! lengths, batch bounds, which sources count as raw imports, the entity
//! fan-out ceiling, and the keyset cursor's meaning.

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineTables};
use super::{Result, Store};
use crate::db::queries::{parse_memory_row, MEMORY_COLUMNS};
use crate::models::{ContradictionSide, Memory};
#[cfg(feature = "engine-store")]
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};

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

/// The curation reads, over one connection, or on the engine's
/// memories core when the store's tables hold it (`db::engine::curation`).
pub struct Curation<'c> {
    conn: &'c Connection,
    #[cfg(feature = "engine-store")]
    core: Option<&'c Mutex<EngineTables>>,
}

impl<'c> Curation<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            #[cfg(feature = "engine-store")]
            core: store.core(),
        }
    }

    fn count(&self, sql: &str) -> Result<i64> {
        Ok(self.conn.query_row(sql, [], |r| r.get(0))?)
    }

    // --- captures --------------------------------------------------------

    /// Every memory carrying `capture_id`, by category (ties by id).
    pub fn capture_rows(&self, capture_id: &str) -> Result<Vec<Memory>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::capture_rows(&core.lock(), capture_id);
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {MEMORY_COLUMNS} FROM memories WHERE capture_id = ? ORDER BY category, id"
        ))?;
        let rows = stmt
            .query_map(params![capture_id], parse_memory_row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// The tags of the lowest-id memory carrying `capture_id`, or `None` when
    /// none does. Unparseable tags read as none.
    pub fn capture_tags(&self, capture_id: &str) -> Result<Option<Vec<String>>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::capture_tags(&core.lock(), capture_id);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT tags FROM memories WHERE capture_id = ? ORDER BY id LIMIT 1",
                params![capture_id],
                |row| {
                    let tags_json: String = row.get(0)?;
                    Ok(serde_json::from_str(&tags_json).unwrap_or_default())
                },
            )
            .optional()?)
    }

    /// Captures nothing has been decomposed from, newest first, at most
    /// `limit`.
    pub fn undecomposed(&self, limit: usize) -> Result<Vec<CaptureRow>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::undecomposed(&core.lock(), limit);
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id, m.capture_id, m.content, m.category, m.tags
               FROM memories m
              WHERE {UNDECOMPOSED_CAPTURE}
              ORDER BY m.created_at DESC, m.id DESC
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
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// How many captures nothing has been decomposed from.
    pub fn count_undecomposed(&self) -> Result<i64> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::count_undecomposed(&core.lock());
        }
        self.count(&format!(
            "SELECT count(*) FROM memories m WHERE {UNDECOMPOSED_CAPTURE}"
        ))
    }

    /// How many distinct live captures there are, and the newest one's
    /// `created_at`.
    pub fn capture_activity(&self) -> Result<CaptureActivity> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::capture_activity(&core.lock());
        }
        Ok(self.conn.query_row(
            "SELECT COUNT(DISTINCT capture_id), MAX(created_at) FROM memories
              WHERE capture_id IS NOT NULL AND deleted_at IS NULL",
            [],
            |r| {
                Ok(CaptureActivity {
                    captures: r.get(0)?,
                    last_capture_at: r.get(1)?,
                })
            },
        )?)
    }

    // --- normalization ---------------------------------------------------

    /// Live raw imports from `sources` nothing has been normalized from,
    /// newest first, at most `limit`. Unparseable metadata reads as `{}`.
    pub fn unnormalized(&self, sources: &[&str], limit: usize) -> Result<Vec<ImportRow>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::unnormalized(&core.lock(), sources, limit);
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id, m.content, m.category, m.source, m.tags, m.metadata
               FROM memories m
              WHERE {}
              ORDER BY m.created_at DESC, m.id DESC
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
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// How many live raw imports from `sources` nothing has been
    /// normalized from.
    pub fn count_unnormalized(&self, sources: &[&str]) -> Result<i64> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::count_unnormalized(&core.lock(), sources);
        }
        self.count(&format!(
            "SELECT count(*) FROM memories m WHERE {}",
            unnormalized_where(sources)
        ))
    }

    /// What a normalization of `memory_id` copies from it, if it exists.
    pub fn normalization_source(&self, memory_id: &str) -> Result<Option<NormalizationSource>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::normalization_source(&core.lock(), memory_id);
        }
        Ok(self
            .conn
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
            .optional()?)
    }

    // --- maintenance -----------------------------------------------------

    /// How deep `backlog` is.
    pub fn backlog_depth(&self, backlog: Backlog) -> Result<i64> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::backlog_depth(&core.lock(), backlog);
        }
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
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::count_contradiction_pairs(&core.lock(), max_fanout);
        }
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
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::contradiction_pairs(&core.lock(), max_fanout, cursor, limit);
        }
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
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// The names of the entities both memories mention, by name.
    pub fn shared_entity_names(&self, id_a: &str, id_b: &str) -> Result<Vec<String>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::shared_entity_names(&core.lock(), id_a, id_b);
        }
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT e.name FROM memory_entities me1
               JOIN memory_entities me2 ON me2.entity_id = me1.entity_id
               JOIN entities e ON e.id = me1.entity_id
              WHERE me1.memory_id = ? AND me2.memory_id = ?
              ORDER BY e.name",
        )?;
        let rows = stmt
            .query_map(params![id_a, id_b], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// One side of a contradiction pair, with the first `snippet_chars`
    /// characters of its content.
    pub fn contradiction_side(
        &self,
        memory_id: &str,
        snippet_chars: usize,
    ) -> Result<ContradictionSide> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::curation::contradiction_side(&core.lock(), memory_id, snippet_chars);
        }
        Ok(self.conn.query_row(
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
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    /// Captures, imports, graph links and promotions on `db`, and every
    /// curation and promotion read over them.
    fn exercise(db: &Database) -> Vec<String> {
        use crate::db::derived::Origin;
        use crate::db::entities::Entities;
        use crate::db::promotions::Promotions;
        use crate::entity::Entity;
        const T2: &str = "2026-09-27T00:00:00+00:00";
        let store = db.store();
        let memories = Memories::new(&store);
        let text = |s: &str| Some(s.to_string());
        let rows: Vec<NewMemory> = vec![
            NewMemory {
                capture_id: text("c1"),
                category: "dialog".into(),
                tags: vec!["x".into()],
                ..NewMemory::new("d1", "dialog one", NOW)
            },
            NewMemory {
                capture_id: text("c1"),
                category: "summary".into(),
                ..NewMemory::new("s1", "summary one", NOW)
            },
            NewMemory {
                capture_id: text("c2"),
                category: "dialog".into(),
                ..NewMemory::new("d2", "dialog two", T2)
            },
            NewMemory {
                source_capture_id: text("c2"),
                category: "fact".into(),
                subject: text(" Rust "),
                predicate: text("IS"),
                object: text("fast"),
                vitality: 0.9,
                ..NewMemory::new("f1", "fact one ü", T2)
            },
            NewMemory {
                category: "fact".into(),
                subject: text("rust"),
                predicate: text("is"),
                object: text("safe"),
                vitality: 0.9,
                ..NewMemory::new("f2", "fact two", NOW)
            },
            NewMemory {
                category: "fact".into(),
                vitality: 0.4,
                sensitive: true,
                ..NewMemory::new("f3", "fact three", NOW)
            },
            NewMemory {
                category: "fact".into(),
                vitality: 0.7,
                ..NewMemory::new("f4", "fact four", T2)
            },
            NewMemory {
                source: "chat_import".into(),
                metadata: serde_json::json!({"k": 1}),
                ..NewMemory::new("raw1", "raw one", NOW)
            },
            NewMemory {
                source: "document_import".into(),
                doc_id: text("doc"),
                chunk_index: Some(2),
                ..NewMemory::new("raw2", "raw two", T2)
            },
            NewMemory {
                metadata: serde_json::json!({"normalized_from": "raw1"}),
                ..NewMemory::new("n1", "norm", T2)
            },
            NewMemory {
                category: "scenario".into(),
                vitality: 0.8,
                ..NewMemory::new("sc1", "scenario one", NOW)
            },
            NewMemory {
                category: "scenario".into(),
                vitality: 0.8,
                ..NewMemory::new("sc2", "scenario two", NOW)
            },
            NewMemory {
                category: "scenario".into(),
                vitality: 0.2,
                ..NewMemory::new("sc3", "scenario three", NOW)
            },
            NewMemory {
                category: "persona".into(),
                vitality: 0.5,
                sensitive: true,
                ..NewMemory::new("p1", "persona", T2)
            },
        ];
        for row in &rows {
            memories.insert(row).unwrap();
        }
        let entities = Entities::new(&store);
        for (id, name) in [("e1", "Rust"), ("e2", "Tokio"), ("e3", "Hub")] {
            entities
                .insert(
                    &Entity {
                        id: id.into(),
                        name: name.into(),
                        kind: None,
                        aliases: vec![],
                        created_at: NOW.into(),
                        updated_at: NOW.into(),
                    },
                    None,
                )
                .unwrap();
        }
        for (m, e) in [
            ("f1", "e1"),
            ("f2", "e1"),
            ("f3", "e1"),
            ("f4", "e1"),
            ("f1", "e2"),
            ("f4", "e2"),
            ("d1", "e2"),
            ("f2", "e3"),
            ("f4", "e3"),
            ("sc1", "e3"),
            ("ghost", "e3"),
        ] {
            entities.link(m, e, NOW, Origin::Local).unwrap();
        }
        let promotions = Promotions::new(&store);
        for (p, src, rung) in [
            ("sc1", "f4", "scenario"),
            ("sc1", "f2", "scenario"),
            ("sc1", "f2", "scenario"),
            ("p1", "sc1", "persona"),
            ("p1", "gone", "persona"),
        ] {
            promotions.record(p, src, rung, NOW).unwrap();
        }
        memories.set_superseded_by("f2", "f1", None).unwrap();

        let curation = Curation::new(&store);
        let sources = ["document_import", "chat_import"];
        let mut seen = vec![
            format!(
                "{:?}",
                curation
                    .capture_rows("c1")
                    .unwrap()
                    .iter()
                    .map(|m| &m.id)
                    .collect::<Vec<_>>()
            ),
            format!("{:?}", curation.capture_tags("c1").unwrap()),
            format!("{:?}", curation.capture_tags("none").unwrap()),
            format!("{:?}", curation.undecomposed(10).unwrap()),
            format!("{}", curation.count_undecomposed().unwrap()),
            format!("{:?}", curation.capture_activity().unwrap()),
            format!("{:?}", curation.unnormalized(&sources, 10).unwrap()),
            format!("{}", curation.count_unnormalized(&sources).unwrap()),
            format!("{:?}", curation.normalization_source("raw2").unwrap()),
            format!("{:?}", curation.normalization_source("missing").unwrap()),
        ];
        for backlog in [
            Backlog::Undecomposed,
            Backlog::Unannotated,
            Backlog::Unnormalized,
            Backlog::Unclassified,
        ] {
            seen.push(format!("{}", curation.backlog_depth(backlog).unwrap()));
        }
        for fanout in [20, 3] {
            seen.push(format!(
                "{}",
                curation.count_contradiction_pairs(fanout).unwrap()
            ));
            seen.push(format!(
                "{:?}",
                curation.contradiction_pairs(fanout, None, 2).unwrap()
            ));
            seen.push(format!(
                "{:?}",
                curation
                    .contradiction_pairs(fanout, Some(("f1", "f3")), 10)
                    .unwrap()
            ));
        }
        seen.push(format!(
            "{:?}",
            curation.shared_entity_names("f1", "f4").unwrap()
        ));
        seen.push(format!(
            "{:?}",
            curation.contradiction_side("f1", 5).unwrap()
        ));
        seen.push(format!(
            "{}",
            curation.contradiction_side("missing", 5).is_err()
        ));

        seen.push(format!(
            "{:?}",
            promotions.undecomposed_dialogs(10).unwrap()
        ));
        seen.push(format!(
            "{}",
            promotions.count_undecomposed_dialogs().unwrap()
        ));
        seen.push(format!(
            "{:?}",
            promotions
                .entity_fact_groups("fact", "scenario", 2, 10)
                .unwrap()
        ));
        seen.push(format!(
            "{:?}",
            promotions
                .entity_fact_groups("fact", "other", 1, 1)
                .unwrap()
        ));
        seen.push(format!(
            "{}",
            promotions
                .count_entity_fact_groups("fact", "other", 1)
                .unwrap()
        ));
        seen.push(format!(
            "{:?}",
            promotions
                .ready_scenarios("scenario", 0.5, "persona", 10)
                .unwrap()
        ));
        seen.push(format!(
            "{}",
            promotions
                .count_ready_scenarios("scenario", 0.5, "persona")
                .unwrap()
        ));
        seen.push(format!(
            "{:?}",
            promotions.live_source_sensitivity("f3").unwrap()
        ));
        seen.push(format!(
            "{:?}",
            promotions.live_source_sensitivity("f2").unwrap()
        ));
        seen.push(format!(
            "{:?}",
            promotions.promoted_from("f2", "scenario").unwrap()
        ));
        seen.push(format!(
            "{:?}",
            promotions.sources_at("sc1", "scenario").unwrap()
        ));
        seen.push(format!(
            "{} {}",
            promotions.is_live("f1").unwrap(),
            promotions.is_live("f2").unwrap()
        ));
        seen.push(format!("{:?}", promotions.sources_of("p1").unwrap()));
        seen.push(format!("{:?}", promotions.derived_from("sc1").unwrap()));
        seen.push(format!("{}", promotions.surviving_sources("sc1").unwrap()));
        seen.push(format!(
            "{:?}",
            promotions.statements_by_vitality("fact").unwrap()
        ));
        seen.push(format!(
            "{:?}",
            promotions.statements_newest_first("fact").unwrap()
        ));
        seen
    }

    #[test]
    fn the_engine_core_curates_and_promotes_as_sqlite_does() {
        let mut observed = Vec::new();
        crate::db::on_each_backend(|db| observed.push(exercise(db)));
        let sqlite = &observed[0];
        assert_ne!(sqlite[14], "0", "the corpus has contradiction pairs");
        for other in &observed[1..] {
            for (theirs, ours) in other.iter().zip(sqlite) {
                assert_eq!(theirs, ours);
            }
            assert_eq!(other.len(), sqlite.len());
        }
    }

    #[test]
    fn a_capture_leaves_the_backlog_once_a_fact_names_it() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        memories
            .insert(&NewMemory {
                capture_id: Some("cap".to_string()),
                ..NewMemory::new("dialog", "x", NOW)
            })
            .unwrap();
        let curation = Curation::new(&store);
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
        let store = db.store();
        let memories = Memories::new(&store);
        memories
            .insert(&NewMemory {
                source: "chat_import".to_string(),
                ..NewMemory::new("raw", "x", NOW)
            })
            .unwrap();
        let curation = Curation::new(&store);
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
        let store = db.store();
        let memories = Memories::new(&store);
        let entities = crate::db::entities::Entities::new(&store);
        for id in ["a", "b", "c"] {
            memories.insert(&NewMemory::new(id, id, NOW)).unwrap();
            entities
                .link(id, "e", NOW, crate::db::derived::Origin::Local)
                .unwrap();
        }
        let curation = Curation::new(&store);
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
