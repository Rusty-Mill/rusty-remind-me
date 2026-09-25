//! Storage for feedback: the `memory_feedback` event log, the importance
//! columns a global judgement rewrites (`base_weight`, `vitality`, `status`),
//! and the recalibration queries that look for memories never given any.
//!
//! Every statement for feedback lives here (ADR-0022). The rules stay where
//! they were: the feedback maths and the similarity threshold in
//! [`crate::vitality`], and what makes a memory due for review in
//! [`crate::recalibrate`], which passes its thresholds in.

use crate::models::RecalibrateCandidate;
use rusqlite::{params, params_from_iter, Connection, Result};

/// The importance columns of a live memory, as a global judgement reads
/// them before rewriting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Importance {
    pub access_count: i64,
    pub base_weight: f64,
    pub vitality: f64,
}

/// One logged feedback event, as the read side needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedbackEvent {
    /// The event's query, tokenised and space-joined.
    pub query_tokens: String,
    /// `helpful` or `unhelpful`.
    pub signal: String,
    pub magnitude: f64,
}

/// What makes a memory due for an importance review, apart from never
/// having had feedback. Thresholds come from [`crate::recalibrate`].
#[derive(Debug, Clone, Copy)]
pub struct ReviewFilter<'a> {
    /// A memory at or above this `base_weight` qualifies...
    pub min_base_weight: f64,
    /// ...as does one of these `memory_type`s.
    pub durable_types: &'a [&'a str],
    /// Days since last access (or creation, if never accessed).
    pub stale_days: i64,
}

/// The feedback tables, over one connection.
pub struct Feedback<'c> {
    conn: &'c Connection,
}

impl<'c> Feedback<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// `memory_id`'s importance columns, or `None` if it is missing or
    /// deleted.
    pub fn importance(&self, memory_id: &str) -> Result<Option<Importance>> {
        let mut stmt = self.conn.prepare(
            "SELECT access_count, base_weight, vitality FROM memories
             WHERE id = ? AND deleted_at IS NULL",
        )?;
        let mut rows = stmt.query([memory_id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(Importance {
            access_count: row.get(0)?,
            base_weight: row.get(1)?,
            vitality: row.get(2)?,
        }))
    }

    /// Rewrite `memory_id`'s importance after a global judgement.
    pub fn set_importance(
        &self,
        memory_id: &str,
        base_weight: f64,
        vitality: f64,
        status: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE memories SET base_weight = ?, vitality = ?, status = ? WHERE id = ?",
            params![base_weight, vitality, status, memory_id],
        )?;
        Ok(())
    }

    /// Log a query-contextual feedback event under `id`.
    pub fn log_event(
        &self,
        id: &str,
        memory_id: &str,
        query: &str,
        event: &FeedbackEvent,
        created_at: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO memory_feedback
                (id, memory_id, query, query_tokens, signal, magnitude, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                id,
                memory_id,
                query,
                event.query_tokens,
                event.signal,
                event.magnitude,
                created_at
            ],
        )?;
        Ok(())
    }

    /// Every feedback event logged for `memory_id`.
    pub fn events(&self, memory_id: &str) -> Result<Vec<FeedbackEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT query_tokens, signal, magnitude FROM memory_feedback WHERE memory_id = ?",
        )?;
        let rows = stmt
            .query_map([memory_id], |row| {
                Ok(FeedbackEvent {
                    query_tokens: row.get(0)?,
                    signal: row.get(1)?,
                    magnitude: row.get(2)?,
                })
            })?
            .collect();
        rows
    }

    /// How many memories `filter` makes due for review.
    pub fn review_count(&self, filter: &ReviewFilter<'_>) -> Result<i64> {
        let (predicate, bindings) = review_where(filter);
        self.conn.query_row(
            &format!("SELECT COUNT(*) FROM memories m WHERE {predicate}"),
            params_from_iter(bindings.iter()),
            |r| r.get(0),
        )
    }

    /// Up to `limit` memories `filter` makes due for review, heaviest and
    /// longest-unread first, their content cut to `snippet_chars`.
    pub fn review_batch(
        &self,
        filter: &ReviewFilter<'_>,
        snippet_chars: usize,
        limit: i64,
    ) -> Result<Vec<RecalibrateCandidate>> {
        let (predicate, mut bindings) = review_where(filter);
        bindings.push(rusqlite::types::Value::Integer(limit));
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, substr(content, 1, {snippet_chars}) AS content_snippet, category,
                    memory_type, base_weight, access_count, accessed_at, created_at
               FROM memories m
              WHERE {predicate}
              ORDER BY m.base_weight DESC, m.accessed_at ASC
              LIMIT ?"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(bindings.iter()), |r| {
                Ok(RecalibrateCandidate {
                    id: r.get(0)?,
                    content_snippet: r.get(1)?,
                    category: r.get(2)?,
                    memory_type: r.get(3)?,
                    base_weight: r.get(4)?,
                    access_count: r.get(5)?,
                    accessed_at: r.get(6)?,
                    created_at: r.get(7)?,
                })
            })?
            .collect();
        rows
    }
}

/// The review predicate over `memories m`, and the values it binds in order.
/// Live, unsuperseded memories that are weighty or of a durable type, unread
/// for `stale_days`, and never given feedback.
fn review_where(filter: &ReviewFilter<'_>) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;
    // SQLite accepts an empty `IN ()`, which matches nothing.
    let type_slots = vec!["?"; filter.durable_types.len()].join(", ");
    let predicate = format!(
        "m.superseded_by IS NULL
         AND m.deleted_at IS NULL
         AND (m.base_weight >= ? OR m.memory_type IN ({type_slots}))
         AND (julianday('now') - julianday(COALESCE(m.accessed_at, m.created_at))) >= ?
         AND NOT EXISTS (SELECT 1 FROM memory_feedback mf WHERE mf.memory_id = m.id)"
    );
    let mut bindings = vec![Value::Real(filter.min_base_weight)];
    bindings.extend(
        filter
            .durable_types
            .iter()
            .map(|t| Value::Text((*t).to_string())),
    );
    bindings.push(Value::Integer(filter.stale_days));
    (predicate, bindings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn review_needs_weight_or_a_durable_type_and_no_feedback() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let old = "2020-01-01T00:00:00+00:00";
        for (id, weight, kind) in [
            ("heavy", 2.0, "note"),
            ("fact", 0.5, "fact"),
            ("light", 0.5, "note"),
        ] {
            conn.execute(
                "INSERT INTO memories (id, content, base_weight, memory_type, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![id, format!("content {id}"), weight, kind, old, old],
            )
            .unwrap();
        }
        let feedback = Feedback::new(&conn);
        let filter = ReviewFilter {
            min_base_weight: 1.0,
            durable_types: &["fact"],
            stale_days: 30,
        };
        assert_eq!(feedback.review_count(&filter).unwrap(), 2);
        let ids: Vec<String> = feedback
            .review_batch(&filter, 10, 10)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["heavy", "fact"], "heaviest first");

        let event = FeedbackEvent {
            query_tokens: "q".into(),
            signal: "helpful".into(),
            magnitude: 0.1,
        };
        feedback
            .log_event("fb_1", "heavy", "q", &event, old)
            .unwrap();
        assert_eq!(feedback.events("heavy").unwrap(), vec![event]);
        assert_eq!(
            feedback.review_count(&filter).unwrap(),
            1,
            "feedback settles a memory"
        );

        let no_types = ReviewFilter {
            durable_types: &[],
            ..filter
        };
        assert_eq!(
            feedback.review_count(&no_types).unwrap(),
            0,
            "an empty type list matches nothing"
        );
    }
}
