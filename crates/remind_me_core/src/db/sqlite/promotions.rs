//! Promotion provenance and the promotion reads on SQLite: the
//! [`PromotionStore`] adapter.

use super::memories::{rows_where, text};
use super::SqliteTables;
use crate::db::engine::curation::undecomposed_rows_in;
use crate::db::engine::memories::MemoryRow;
use crate::db::promotions::{
    EntityFactGroup, IdContent, PromotionStore, ScoredMemory, StatementRow,
};
use crate::db::Result;
use rusqlite::params;
use std::collections::HashSet;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS promotions (
    promoted_id TEXT NOT NULL, source_id TEXT NOT NULL, rung TEXT NOT NULL,
    promoted_at TEXT NOT NULL, PRIMARY KEY (promoted_id, source_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS promotions_source ON promotions (source_id);
";

const LIVE: &str = "deleted_at IS NULL AND superseded_by IS NULL";

/// Whether memory `m` is a decision its own outcome discredits.
const DISCREDITED: &str =
    "(m.memory_type = 'decision' AND coalesce(m.outcome, '') IN ('reverted', 'abandoned'))";

impl SqliteTables {
    fn ids_of(&self, sql: &str, a: &str, b: Option<&str>) -> Result<Vec<String>> {
        self.all(sql, params![a, b], |r| r.get(0))
    }

    fn undecomposed_dialogs_rows(&self) -> Result<Vec<MemoryRow>> {
        let rows = rows_where(&self.conn, "1", "id", &[])?;
        let mut rows: Vec<MemoryRow> = undecomposed_rows_in(|| rows.iter().cloned(), None)
            .into_iter()
            .filter(|row| row.category == "dialog")
            .collect();
        rows.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));
        Ok(rows)
    }

    fn fact_groups(
        &self,
        category: &str,
        rung: &str,
        min_facts: usize,
        min_sources: usize,
    ) -> Result<Vec<EntityFactGroup>> {
        let links: Vec<(String, String, String, String)> = self.all(
            "SELECT l.entity_id, e.name, m.id, coalesce(m.source_capture_id, m.id) \
             FROM memory_entities l JOIN entities e ON e.id = l.entity_id \
             JOIN memories m ON m.id = l.memory_id \
             WHERE m.deleted_at IS NULL AND m.superseded_by IS NULL AND m.category = ?1 \
             AND m.id NOT IN (SELECT source_id FROM promotions WHERE rung = ?2) \
             ORDER BY l.entity_id, m.id",
            [category, rung],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let mut groups: Vec<(String, String, Vec<String>, HashSet<String>)> = Vec::new();
        for (entity_id, name, memory_id, origin) in links {
            match groups.last_mut() {
                Some(g) if g.0 == entity_id => {
                    g.2.push(memory_id);
                    g.3.insert(origin);
                }
                _ => groups.push((entity_id, name, vec![memory_id], HashSet::from([origin]))),
            }
        }
        let mut found: Vec<_> = groups
            .into_iter()
            .filter(|g| g.2.len() >= min_facts && g.3.len() >= min_sources)
            .collect();
        found.sort_by(|a, b| b.2.len().cmp(&a.2.len()).then_with(|| a.0.cmp(&b.0)));
        Ok(found
            .into_iter()
            .map(|(_, entity_name, memory_ids, _)| EntityFactGroup {
                entity_name,
                memory_ids,
            })
            .collect())
    }

    fn scenarios(&self, category: &str, floor: f64, rung: &str) -> Result<Vec<MemoryRow>> {
        // A scenario resting on a reverted or abandoned decision is withheld.
        rows_where(
            &self.conn,
            &format!(
                "{LIVE} AND sensitive = 0 AND category = ?1 AND vitality >= ?2 \
                 AND id NOT IN (SELECT source_id FROM promotions WHERE rung = ?3) \
                 AND id NOT IN (SELECT p.promoted_id FROM promotions p JOIN memories m \
                     ON m.id = p.source_id WHERE {DISCREDITED})"
            ),
            "vitality DESC, id",
            &[text(category), floor.into(), text(rung)],
        )
    }

    fn statements(&self, category: &str, by_vitality: bool) -> Result<Vec<StatementRow>> {
        let (extra, order) = match by_vitality {
            true => (" AND sensitive = 0", "vitality DESC, id"),
            false => ("", "created_at DESC, id"),
        };
        let rows = rows_where(
            &self.conn,
            &format!("{LIVE} AND category = ?1{extra}"),
            order,
            &[text(category)],
        )?;
        Ok(rows
            .into_iter()
            .map(|row| StatementRow {
                // Not JSON reads as no metadata, so no pin, as `json_extract` would.
                metadata: row.metadata_value().unwrap_or(serde_json::Value::Null),
                id: row.id,
                content: row.content,
                vitality: row.vitality,
                created_at: row.created_at,
                written_by: row.written_by,
            })
            .collect())
    }
}

impl PromotionStore for SqliteTables {
    fn undecomposed_dialogs(&self, n: usize) -> Result<Vec<IdContent>> {
        Ok(self
            .undecomposed_dialogs_rows()?
            .into_iter()
            .take(n)
            .map(|row| IdContent {
                id: row.id,
                content: row.content,
            })
            .collect())
    }

    fn count_undecomposed_dialogs(&self) -> Result<usize> {
        Ok(self.undecomposed_dialogs_rows()?.len())
    }

    fn entity_fact_groups(
        &self,
        category: &str,
        rung: &str,
        min_facts: usize,
        min_sources: usize,
        n: usize,
    ) -> Result<Vec<EntityFactGroup>> {
        let mut groups = self.fact_groups(category, rung, min_facts, min_sources)?;
        groups.truncate(n);
        Ok(groups)
    }

    fn count_entity_fact_groups(
        &self,
        category: &str,
        rung: &str,
        min_facts: usize,
        min_sources: usize,
    ) -> Result<usize> {
        Ok(self
            .fact_groups(category, rung, min_facts, min_sources)?
            .len())
    }

    fn ready_scenarios(
        &self,
        category: &str,
        floor: f64,
        rung: &str,
        n: usize,
    ) -> Result<Vec<ScoredMemory>> {
        Ok(self
            .scenarios(category, floor, rung)?
            .into_iter()
            .take(n)
            .map(|row| ScoredMemory {
                id: row.id,
                content: row.content,
                vitality: row.vitality,
            })
            .collect())
    }

    fn count_ready_scenarios(&self, category: &str, floor: f64, rung: &str) -> Result<usize> {
        Ok(self.scenarios(category, floor, rung)?.len())
    }

    fn live_source_sensitivity(&self, id: &str) -> Result<Option<bool>> {
        self.one(
            &format!("SELECT sensitive FROM memories WHERE id = ?1 AND {LIVE}"),
            [id],
            |r| r.get(0),
        )
    }

    fn promoted_from(&self, source_id: &str, rung: &str) -> Result<Vec<String>> {
        self.ids_of(
            "SELECT DISTINCT promoted_id FROM promotions WHERE source_id = ?1 AND rung = ?2 ORDER BY 1",
            source_id,
            Some(rung),
        )
    }

    fn sources_at(&self, promoted_id: &str, rung: &str) -> Result<Vec<String>> {
        self.ids_of(
            "SELECT source_id FROM promotions WHERE promoted_id = ?1 AND rung = ?2 ORDER BY 1",
            promoted_id,
            Some(rung),
        )
    }

    fn is_live(&self, id: &str) -> Result<bool> {
        Ok(self.live_source_sensitivity(id)?.is_some())
    }

    fn record(&mut self, promoted_id: &str, source_id: &str, rung: &str, at: &str) -> Result<()> {
        self.exec(
            "INSERT OR IGNORE INTO promotions VALUES (?1, ?2, ?3, ?4)",
            params![promoted_id, source_id, rung, at],
        )
        .map(drop)
    }

    fn sources_of(&self, memory_id: &str) -> Result<Vec<String>> {
        self.ids_of(
            "SELECT source_id FROM promotions WHERE promoted_id = ?1 AND ?2 IS NULL ORDER BY 1",
            memory_id,
            None,
        )
    }

    fn derived_from(&self, memory_id: &str) -> Result<Vec<String>> {
        self.ids_of(
            "SELECT promoted_id FROM promotions WHERE source_id = ?1 AND ?2 IS NULL ORDER BY 1",
            memory_id,
            None,
        )
    }

    fn surviving_sources(&self, promoted_id: &str) -> Result<usize> {
        let n: i64 = self
            .one(
                &format!(
                    "SELECT count(*) FROM promotions p JOIN memories m ON m.id = p.source_id \
                     WHERE p.promoted_id = ?1 AND m.deleted_at IS NULL AND m.superseded_by IS NULL \
                     AND NOT {DISCREDITED}"
                ),
                [promoted_id],
                |r| r.get(0),
            )?
            .unwrap_or(0);
        Ok(usize::try_from(n).unwrap_or(0))
    }

    fn statements_by_vitality(&self, category: &str) -> Result<Vec<StatementRow>> {
        self.statements(category, true)
    }

    fn statements_newest_first(&self, category: &str) -> Result<Vec<StatementRow>> {
        self.statements(category, false)
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::promotions::promotion_conformance!(SqliteTables::open_in_memory().unwrap());
}
