//! The curation queues on SQLite: the [`CurationStore`] adapter. The
//! queue rules are the engine's (`db::engine::curation`), run over rows
//! read from SQLite.

use super::memories::{rows_where, text};
use super::SqliteTables;
use crate::db::curation::{
    Backlog, CaptureActivity, CaptureRow, CurationStore, ImportRow, NormalizationSource,
};
use crate::db::engine::curation::{
    capture_revisions, count_settled_captures, drawn_at, pairs_in, same_claim, same_project,
    standing_decision, tags, undecomposed_rows_in, unnormalized_in,
};
use crate::db::engine::memories::MemoryRow;
use crate::db::{Result, StoreError};
use crate::models::{ContradictionSide, Memory};
use std::collections::{BTreeMap, BTreeSet};

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl SqliteTables {
    fn every_row(&self) -> Result<Vec<MemoryRow>> {
        rows_where(&self.conn, "1", "id", &[])
    }

    fn link_pairs(&self) -> Result<Vec<(String, String)>> {
        self.all(
            "SELECT memory_id, entity_id FROM memory_entities",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    fn undecomposed(&self, settled_before: Option<&str>) -> Result<Vec<MemoryRow>> {
        let rows = self.every_row()?;
        Ok(undecomposed_rows_in(
            || rows.iter().cloned(),
            settled_before,
        ))
    }

    fn unnormalized(&self, sources: &[&str]) -> Result<Vec<MemoryRow>> {
        let rows = self.every_row()?;
        Ok(unnormalized_in(|| rows.iter().cloned(), sources))
    }

    fn pairs(&self, max_fanout: i64) -> Result<BTreeSet<(String, String)>> {
        let rows: BTreeMap<String, MemoryRow> = self
            .every_row()?
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();
        Ok(pairs_in(
            self.link_pairs()?,
            |id| rows.get(id).cloned(),
            max_fanout,
        ))
    }
}

impl CurationStore for SqliteTables {
    fn capture_rows(&self, capture_id: &str) -> Result<Vec<Memory>> {
        let rows = rows_where(
            &self.conn,
            "capture_id = ?1",
            "category, id",
            &[text(capture_id)],
        )?;
        Ok(rows.iter().map(MemoryRow::to_memory).collect())
    }

    fn capture_tags(&self, capture_id: &str) -> Result<Option<Vec<String>>> {
        let rows = rows_where(
            &self.conn,
            "capture_id = ?1",
            "id LIMIT 1",
            &[text(capture_id)],
        )?;
        Ok(rows.first().map(tags))
    }

    fn undecomposed(&self, n: usize, settled_before: Option<&str>) -> Result<Vec<CaptureRow>> {
        let revisions = capture_revisions(self.every_row()?.into_iter());
        Ok(SqliteTables::undecomposed(self, settled_before)?
            .into_iter()
            .take(n)
            .map(|row| CaptureRow {
                tags: tags(&row),
                revision: row
                    .capture_id
                    .as_ref()
                    .and_then(|c| revisions.get(c).cloned())
                    .unwrap_or_else(|| row.updated_at.clone()),
                capture_id: row.capture_id.unwrap_or_default(),
                id: row.id,
                content: row.content,
                category: row.category,
            })
            .collect())
    }

    fn count_undecomposed(&self, settled_before: Option<&str>) -> Result<i64> {
        Ok(count(
            SqliteTables::undecomposed(self, settled_before)?.len(),
        ))
    }

    fn count_settled_captures(&self, settled_before: Option<&str>) -> Result<i64> {
        // Only the columns the rule reads: no content, tags or full rows.
        let facts: Vec<(String, String)> = self.all(
            "SELECT source_capture_id, metadata, created_at FROM memories \
             WHERE source_capture_id IS NOT NULL",
            [],
            |r| {
                let (metadata, created_at): (String, String) = (r.get(1)?, r.get(2)?);
                Ok((r.get(0)?, drawn_at(&metadata, &created_at)))
            },
        )?;
        let captures: Vec<(String, String)> = self.all(
            "SELECT capture_id, updated_at FROM memories WHERE capture_id IS NOT NULL \
             AND source_capture_id IS NULL AND deleted_at IS NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(count(count_settled_captures(
            facts.into_iter(),
            captures.into_iter(),
            settled_before,
        )))
    }

    fn capture_activity(&self) -> Result<CaptureActivity> {
        Ok(self
            .one(
                "SELECT count(DISTINCT capture_id), max(created_at) FROM memories \
                 WHERE deleted_at IS NULL AND capture_id IS NOT NULL",
                [],
                |r| {
                    Ok(CaptureActivity {
                        captures: r.get(0)?,
                        last_capture_at: r.get(1)?,
                    })
                },
            )?
            .unwrap_or(CaptureActivity {
                captures: 0,
                last_capture_at: None,
            }))
    }

    fn unnormalized(&self, sources: &[&str], n: usize) -> Result<Vec<ImportRow>> {
        Ok(SqliteTables::unnormalized(self, sources)?
            .into_iter()
            .take(n)
            .map(|row| ImportRow {
                tags: tags(&row),
                metadata: serde_json::from_str(&row.metadata)
                    .unwrap_or_else(|_| serde_json::json!({})),
                id: row.id,
                content: row.content,
                category: row.category,
                source: row.source,
            })
            .collect())
    }

    fn count_unnormalized(&self, sources: &[&str]) -> Result<i64> {
        Ok(count(SqliteTables::unnormalized(self, sources)?.len()))
    }

    fn normalization_source(&self, memory_id: &str) -> Result<Option<NormalizationSource>> {
        Ok(self.row(memory_id)?.map(|row| NormalizationSource {
            tags: tags(&row),
            doc_id: row.doc_id,
            chunk_index: row.chunk_index,
        }))
    }

    fn backlog_depth(&self, backlog: Backlog) -> Result<i64> {
        let sql = match backlog {
            Backlog::Undecomposed => return Ok(count(SqliteTables::undecomposed(self, None)?.len())),
            Backlog::Unnormalized => {
                let sources = ["document_import", "chat_import"];
                return Ok(count(SqliteTables::unnormalized(self, &sources)?.len()));
            }
            Backlog::Unannotated => {
                "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND superseded_by IS NULL \
                 AND category != 'dialog' AND subject IS NULL AND predicate IS NULL AND object IS NULL \
                 AND id NOT IN (SELECT memory_id FROM memory_entities)"
            }
            Backlog::Unclassified => {
                "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND memory_type = 'unclassified'"
            }
        };
        Ok(self.one(sql, [], |r| r.get(0))?.unwrap_or(0))
    }

    fn count_contradiction_pairs(&self, max_fanout: i64) -> Result<i64> {
        Ok(count(self.pairs(max_fanout)?.len()))
    }

    fn contradiction_pairs(
        &self,
        max_fanout: i64,
        cursor: Option<(&str, &str)>,
        n: usize,
    ) -> Result<Vec<(String, String)>> {
        Ok(self
            .pairs(max_fanout)?
            .into_iter()
            .filter(|(a, b)| cursor.is_none_or(|(ca, cb)| (a.as_str(), b.as_str()) > (ca, cb)))
            .take(n)
            .collect())
    }

    fn decision_neighbours(
        &self,
        memory_id: &str,
        max_fanout: i64,
    ) -> Result<Vec<(String, Vec<String>)>> {
        let Some(saved) = self.row(memory_id)? else {
            return Ok(Vec::new());
        };
        let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let shared = self.all(
            "SELECT l.memory_id, e.name FROM memory_entities mine \
             JOIN entities e ON e.id = mine.entity_id \
             JOIN memory_entities l ON l.entity_id = mine.entity_id \
             WHERE mine.memory_id = ?1 \
             AND (SELECT count(*) FROM memory_entities f WHERE f.entity_id = mine.entity_id) <= ?2",
            rusqlite::params![memory_id, max_fanout],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        for (id, name) in shared {
            found.entry(id).or_default().insert(name);
        }
        if saved.subject.is_some() && saved.predicate.is_some() {
            let candidates = rows_where(
                &self.conn,
                "subject IS NOT NULL AND predicate IS NOT NULL",
                "id",
                &[],
            )?;
            for row in candidates.into_iter().filter(|row| same_claim(row, &saved)) {
                found.entry(row.id).or_default();
            }
        }
        found.remove(memory_id);
        let mut kept = Vec::new();
        for (id, names) in found {
            let keep = self
                .row(&id)?
                .is_some_and(|row| standing_decision(&row) && same_project(&row, &saved));
            if keep {
                kept.push((id, names.into_iter().collect()));
            }
        }
        Ok(kept)
    }

    fn shared_entity_names(&self, id_a: &str, id_b: &str) -> Result<Vec<String>> {
        self.all(
            "SELECT DISTINCT e.name FROM memory_entities a JOIN memory_entities b ON b.entity_id = a.entity_id \
             JOIN entities e ON e.id = a.entity_id WHERE a.memory_id = ?1 AND b.memory_id = ?2 ORDER BY e.name",
            [id_a, id_b],
            |r| r.get(0),
        )
    }

    fn contradiction_side(
        &self,
        memory_id: &str,
        snippet_chars: usize,
    ) -> Result<ContradictionSide> {
        let row = self.row(memory_id)?.ok_or(StoreError::NotFound)?;
        Ok(ContradictionSide {
            content_snippet: row.content.chars().take(snippet_chars).collect(),
            memory_type: Some(row.memory_type),
            id: row.id,
            category: row.category,
            subject: row.subject,
            predicate: row.predicate,
            object: row.object,
            created_at: row.created_at,
        })
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::curation::curation_conformance!(SqliteTables::open_in_memory().unwrap());
}

#[cfg(test)]
mod tests {
    use super::SqliteTables;
    use crate::db::curation::CurationStore;
    use crate::db::memories::{MemoryStore, NewMemory};

    const OLD: &str = "2026-09-25T00:00:00+00:00";

    /// The settled-capture count reads no `content` or `tags`: with both
    /// columns turned into blobs no `String` decodes from, the full-row
    /// listing fails while the count still answers.
    #[test]
    fn the_settled_count_reads_neither_content_nor_tags() {
        let mut tables = SqliteTables::open_in_memory().unwrap();
        for (id, cap) in [("d", "cap"), ("s", "cap"), ("o", "other")] {
            MemoryStore::insert(
                &mut tables,
                &NewMemory {
                    capture_id: Some(cap.to_string()),
                    ..NewMemory::new(id, "x", OLD)
                },
            )
            .unwrap();
        }
        tables
            .conn
            .execute("UPDATE memories SET content = X'00', tags = X'00'", [])
            .unwrap();

        assert!(CurationStore::undecomposed(&tables, 10, None).is_err());
        assert_eq!(
            CurationStore::count_settled_captures(&tables, None).unwrap(),
            2
        );
    }
}
