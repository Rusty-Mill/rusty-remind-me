//! Feedback events and importance on SQLite: the [`FeedbackStore`] adapter.

use super::memories::rows_where;
use super::{sqlite_error, SqliteTables};
use crate::db::derived::Origin;
use crate::db::engine::feedback::days_since;
use crate::db::engine::memories::MemoryRow;
use crate::db::feedback::{FeedbackEvent, FeedbackStore, Importance, ReviewFilter};
use crate::db::{Result, StoreError};
use crate::models::RecalibrateCandidate;
use rusqlite::params;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memory_feedback (
    id TEXT PRIMARY KEY, memory_id TEXT NOT NULL, query TEXT NOT NULL,
    query_tokens TEXT NOT NULL, signal TEXT NOT NULL CHECK (signal IN ('helpful', 'unhelpful')),
    magnitude REAL NOT NULL, created_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS memory_feedback_memory ON memory_feedback (memory_id);
";

impl SqliteTables {
    /// Rows `filter` makes due for review (see [`FeedbackStore::review_count`]).
    fn due(&self, filter: &ReviewFilter<'_>) -> Result<Vec<MemoryRow>> {
        let now = chrono::Utc::now();
        let rows = rows_where(
            &self.conn,
            "deleted_at IS NULL AND superseded_by IS NULL \
             AND NOT EXISTS (SELECT 1 FROM memory_feedback f WHERE f.memory_id = memories.id)",
            "id",
            &[],
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                row.base_weight >= filter.min_base_weight
                    || filter.durable_types.contains(&row.memory_type.as_str())
            })
            .filter(|row| {
                let last = row.accessed_at.as_deref().unwrap_or(&row.created_at);
                days_since(last, now).is_some_and(|days| days >= filter.stale_days as f64)
            })
            .collect())
    }
}

impl FeedbackStore for SqliteTables {
    fn importance(&self, memory_id: &str) -> Result<Option<Importance>> {
        Ok(self
            .row(memory_id)?
            .filter(|row| row.deleted_at.is_none())
            .map(|row| Importance {
                access_count: row.access_count,
                base_weight: row.base_weight,
                vitality: row.vitality,
            }))
    }

    fn set_importance(
        &mut self,
        id: &str,
        base_weight: f64,
        vitality: f64,
        status: &str,
    ) -> Result<()> {
        self.update_memory(id, Origin::Local, |row| {
            row.base_weight = base_weight;
            row.vitality = vitality;
            row.status = status.to_string();
            Ok(())
        })
    }

    fn log_event(
        &mut self,
        id: &str,
        memory_id: &str,
        query: &str,
        event: &FeedbackEvent,
        created_at: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO memory_feedback VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    memory_id,
                    query,
                    event.query_tokens,
                    event.signal,
                    event.magnitude,
                    created_at
                ],
            )
            .map(drop)
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _)
                    if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    StoreError::Engine(format!("sqlite: feedback {id:?} refused: {e}"))
                }
                e => sqlite_error(e),
            })
    }

    fn events(&self, memory_id: &str) -> Result<Vec<FeedbackEvent>> {
        self.all(
            "SELECT query_tokens, signal, magnitude FROM memory_feedback WHERE memory_id = ?1 \
             ORDER BY created_at, id",
            [memory_id],
            |r| {
                Ok(FeedbackEvent {
                    query_tokens: r.get(0)?,
                    signal: r.get(1)?,
                    magnitude: r.get(2)?,
                })
            },
        )
    }

    fn delete_for(&mut self, memory_id: &str) -> Result<()> {
        self.exec(
            "DELETE FROM memory_feedback WHERE memory_id = ?1",
            [memory_id],
        )
        .map(drop)
    }

    fn review_count(&self, filter: &ReviewFilter<'_>) -> Result<i64> {
        Ok(i64::try_from(self.due(filter)?.len()).unwrap_or(i64::MAX))
    }

    fn review_batch(
        &self,
        filter: &ReviewFilter<'_>,
        snippet_chars: usize,
        limit: usize,
    ) -> Result<Vec<RecalibrateCandidate>> {
        let mut rows = self.due(filter)?;
        // A never-read memory sorts first among equal weights, as NULL does.
        rows.sort_by(|a, b| {
            b.base_weight
                .total_cmp(&a.base_weight)
                .then_with(|| a.accessed_at.cmp(&b.accessed_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(rows
            .into_iter()
            .take(limit)
            .map(|row| RecalibrateCandidate {
                content_snippet: row.content.chars().take(snippet_chars).collect(),
                memory_type: Some(row.memory_type),
                id: row.id,
                category: row.category,
                base_weight: row.base_weight,
                access_count: row.access_count,
                accessed_at: row.accessed_at,
                created_at: row.created_at,
            })
            .collect())
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::feedback::feedback_conformance!(SqliteTables::open_in_memory().unwrap());
}
