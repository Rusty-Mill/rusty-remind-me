//! Storage for promotion: the `promotions` provenance table, and the reads
//! of `memories` and the entity graph that find what is ready to move up a
//! rung.
//!
//! ADR-0023 phase 1, step 5. Every statement [`crate::promotion`] ran lives
//! here. The rules stay there: which rung reads which category, the fact
//! threshold, the persona vitality floor, what makes a source unusable, and
//! that demotion is a read-time judgement.

use rusqlite::{params, Connection, OptionalExtension, Result};

/// Create the `promotions` table and its index, if absent.
///
/// No foreign keys. A promoted memory can be deleted through the ordinary
/// delete path, and a cascade would erase the provenance that says what it
/// was derived from, which is exactly the record needed to explain why a
/// persona statement vanished.
pub fn ensure_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS promotions (
            promoted_id TEXT NOT NULL,
            source_id   TEXT NOT NULL,
            rung        TEXT NOT NULL,
            promoted_at TEXT NOT NULL,
            PRIMARY KEY (promoted_id, source_id)
         );
         CREATE INDEX IF NOT EXISTS idx_promotions_source
            ON promotions(source_id);",
    )
}

/// A memory's id and content.
#[derive(Debug, Clone, PartialEq)]
pub struct IdContent {
    pub id: String,
    pub content: String,
}

/// Facts that mention one entity.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityFactGroup {
    pub entity_name: String,
    pub memory_ids: Vec<String>,
}

/// A scenario and its vitality.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredMemory {
    pub id: String,
    pub content: String,
    pub vitality: f64,
}

/// A persona statement as stored, before its sources are counted.
#[derive(Debug, Clone, PartialEq)]
pub struct StatementRow {
    pub id: String,
    pub content: String,
    pub vitality: f64,
    pub created_at: String,
}

/// The predicate for a dialog nothing has been decomposed from.
const UNDECOMPOSED_DIALOG: &str = "m.capture_id IS NOT NULL
            AND m.source_capture_id IS NULL
            AND m.deleted_at IS NULL
            AND m.category = 'dialog'
            AND NOT EXISTS (
                SELECT 1 FROM memories c WHERE c.source_capture_id = m.capture_id
            )";

/// Live, unsuperseded memories of a category, grouped by an entity they
/// mention, none of them promoted at the rung. Binds category, rung and the
/// minimum group size.
const ENTITY_FACT_GROUPS: &str = "SELECT e.id, e.name, group_concat(m.id), count(*) AS n
           FROM memories m
           JOIN memory_entities me ON me.memory_id = m.id
           JOIN entities e ON e.id = me.entity_id
          WHERE m.category = ?
            AND m.deleted_at IS NULL
            AND m.superseded_by IS NULL
            AND NOT EXISTS (
                SELECT 1 FROM promotions p WHERE p.source_id = m.id AND p.rung = ?
            )
          GROUP BY e.id
         HAVING n >= ?";

/// Live, unsuperseded, non-sensitive memories of a category at or above a
/// vitality, not promoted at the rung. Binds category, floor and rung.
const READY_SCENARIOS: &str = "FROM memories m
          WHERE m.category = ?
            AND m.deleted_at IS NULL
            AND m.superseded_by IS NULL
            AND m.sensitive = 0
            AND m.vitality >= ?
            AND NOT EXISTS (
                SELECT 1 FROM promotions p WHERE p.source_id = m.id AND p.rung = ?
            )";

/// The promotion tables and reads, over one connection.
pub struct Promotions<'c> {
    conn: &'c Connection,
}

impl<'c> Promotions<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    // --- candidates ------------------------------------------------------

    /// Captured dialogs nothing has been decomposed from, newest first, at
    /// most `limit`.
    pub fn undecomposed_dialogs(&self, limit: usize) -> Result<Vec<IdContent>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id, m.content FROM memories m
              WHERE {UNDECOMPOSED_DIALOG}
              ORDER BY m.created_at DESC
              LIMIT ?"
        ))?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(IdContent {
                    id: r.get(0)?,
                    content: r.get(1)?,
                })
            })?
            .collect();
        rows
    }

    /// How many captured dialogs nothing has been decomposed from.
    pub fn count_undecomposed_dialogs(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM memories m WHERE {UNDECOMPOSED_DIALOG}"),
            [],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Groups of at least `min_facts` live `category` memories sharing an
    /// entity, none promoted at `rung`, largest first, at most `limit`.
    pub fn entity_fact_groups(
        &self,
        category: &str,
        rung: &str,
        min_facts: usize,
        limit: usize,
    ) -> Result<Vec<EntityFactGroup>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{ENTITY_FACT_GROUPS} ORDER BY n DESC LIMIT ?"))?;
        let rows = stmt
            .query_map(
                params![category, rung, min_facts as i64, limit as i64],
                |r| {
                    let ids: String = r.get(2)?;
                    Ok(EntityFactGroup {
                        entity_name: r.get(1)?,
                        memory_ids: ids.split(',').map(str::to_string).collect(),
                    })
                },
            )?
            .collect();
        rows
    }

    /// How many groups [`Promotions::entity_fact_groups`] would find
    /// without a limit.
    pub fn count_entity_fact_groups(
        &self,
        category: &str,
        rung: &str,
        min_facts: usize,
    ) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM ({ENTITY_FACT_GROUPS})"),
            params![category, rung, min_facts as i64],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Live, non-sensitive `category` memories at or above `floor`
    /// vitality, not promoted at `rung`, most vital first, at most `limit`.
    pub fn ready_scenarios(
        &self,
        category: &str,
        floor: f64,
        rung: &str,
        limit: usize,
    ) -> Result<Vec<ScoredMemory>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id, m.content, m.vitality {READY_SCENARIOS}
              ORDER BY m.vitality DESC
              LIMIT ?"
        ))?;
        let rows = stmt
            .query_map(params![category, floor, rung, limit as i64], |r| {
                Ok(ScoredMemory {
                    id: r.get(0)?,
                    content: r.get(1)?,
                    vitality: r.get(2)?,
                })
            })?
            .collect();
        rows
    }

    /// How many memories [`Promotions::ready_scenarios`] would find without
    /// a limit.
    pub fn count_ready_scenarios(&self, category: &str, floor: f64, rung: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) {READY_SCENARIOS}"),
            params![category, floor, rung],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    // --- promoting -------------------------------------------------------

    /// Whether the live, unsuperseded memory `id` is sensitive, or `None`
    /// when `id` is not such a memory.
    pub fn live_source_sensitivity(&self, id: &str) -> Result<Option<bool>> {
        self.conn
            .query_row(
                "SELECT sensitive FROM memories
                  WHERE id = ? AND deleted_at IS NULL AND superseded_by IS NULL",
                params![id],
                |r| r.get(0),
            )
            .optional()
    }

    /// The memories promoted at `rung` from a set including `source_id`.
    pub fn promoted_from(&self, source_id: &str, rung: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT promoted_id FROM promotions WHERE source_id = ? AND rung = ?",
        )?;
        let rows = stmt
            .query_map(params![source_id, rung], |r| r.get(0))?
            .collect();
        rows
    }

    /// The sources `promoted_id` was promoted from at `rung`.
    pub fn sources_at(&self, promoted_id: &str, rung: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source_id FROM promotions WHERE promoted_id = ? AND rung = ?")?;
        let rows = stmt
            .query_map(params![promoted_id, rung], |r| r.get(0))?
            .collect();
        rows
    }

    /// Whether `id` is a live, unsuperseded memory.
    pub fn is_live(&self, id: &str) -> Result<bool> {
        let live: i64 = self.conn.query_row(
            "SELECT count(*) FROM memories
              WHERE id = ? AND deleted_at IS NULL AND superseded_by IS NULL",
            params![id],
            |r| r.get(0),
        )?;
        Ok(live > 0)
    }

    /// Record that `promoted_id` was promoted from `source_id` at `rung`.
    /// A pair already recorded is left as it is.
    pub fn record(
        &self,
        promoted_id: &str,
        source_id: &str,
        rung: &str,
        promoted_at: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO promotions (promoted_id, source_id, rung, promoted_at)
             VALUES (?, ?, ?, ?)",
            params![promoted_id, source_id, rung, promoted_at],
        )?;
        Ok(())
    }

    // --- provenance ------------------------------------------------------

    /// Every source `memory_id` was promoted from, at any rung.
    pub fn sources_of(&self, memory_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source_id FROM promotions WHERE promoted_id = ?")?;
        let rows = stmt.query_map(params![memory_id], |r| r.get(0))?.collect();
        rows
    }

    /// Every memory promoted from `memory_id`, at any rung.
    pub fn derived_from(&self, memory_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT promoted_id FROM promotions WHERE source_id = ?")?;
        let rows = stmt.query_map(params![memory_id], |r| r.get(0))?.collect();
        rows
    }

    /// How many of `promoted_id`'s sources are live and unsuperseded.
    pub fn surviving_sources(&self, promoted_id: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*)
               FROM promotions p
               JOIN memories m ON m.id = p.source_id
              WHERE p.promoted_id = ?
                AND m.deleted_at IS NULL
                AND m.superseded_by IS NULL",
            params![promoted_id],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Live, unsuperseded, non-sensitive `category` memories, most vital
    /// first.
    pub fn statements_by_vitality(&self, category: &str) -> Result<Vec<StatementRow>> {
        self.statements(
            "SELECT id, content, vitality, created_at
               FROM memories
              WHERE category = ?
                AND deleted_at IS NULL
                AND superseded_by IS NULL
                AND sensitive = 0
              ORDER BY vitality DESC",
            category,
        )
    }

    /// Live, unsuperseded `category` memories, sensitive or not, newest
    /// first.
    pub fn statements_newest_first(&self, category: &str) -> Result<Vec<StatementRow>> {
        self.statements(
            "SELECT id, content, vitality, created_at
               FROM memories
              WHERE category = ?
                AND deleted_at IS NULL
                AND superseded_by IS NULL
              ORDER BY created_at DESC",
            category,
        )
    }

    fn statements(&self, sql: &str, category: &str) -> Result<Vec<StatementRow>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![category], |r| {
                Ok(StatementRow {
                    id: r.get(0)?,
                    content: r.get(1)?,
                    vitality: r.get(2)?,
                    created_at: r.get(3)?,
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
    fn surviving_sources_counts_only_live_unsuperseded_sources() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let memories = Memories::new(&conn);
        memories.insert(&NewMemory::new("live", "x", NOW)).unwrap();
        memories
            .insert(&NewMemory {
                superseded_by: Some("other".to_string()),
                ..NewMemory::new("old", "x", NOW)
            })
            .unwrap();
        let promotions = Promotions::new(&conn);
        for source in ["live", "old", "missing"] {
            promotions.record("p", source, "rung", NOW).unwrap();
        }
        promotions.record("p", "live", "rung", NOW).unwrap();

        assert_eq!(promotions.surviving_sources("p").unwrap(), 1);
        assert_eq!(promotions.sources_of("p").unwrap().len(), 3);
        assert_eq!(promotions.derived_from("live").unwrap(), vec!["p"]);
    }

    #[test]
    fn live_source_sensitivity_is_none_for_a_superseded_source() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let memories = Memories::new(&conn);
        memories
            .insert(&NewMemory {
                sensitive: true,
                ..NewMemory::new("s", "x", NOW)
            })
            .unwrap();
        memories
            .insert(&NewMemory {
                superseded_by: Some("s".to_string()),
                ..NewMemory::new("old", "x", NOW)
            })
            .unwrap();
        let promotions = Promotions::new(&conn);
        assert_eq!(promotions.live_source_sensitivity("s").unwrap(), Some(true));
        assert_eq!(promotions.live_source_sensitivity("old").unwrap(), None);
        assert!(!promotions.is_live("old").unwrap());
    }
}
