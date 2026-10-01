//! Storage for feedback: the `memory_feedback` event log, the importance
//! columns a global judgement rewrites (`base_weight`, `vitality`, `status`),
//! and the recalibration queries that look for memories never given any.
//!
//! Every statement for feedback lives here (ADR-0022). The rules stay where
//! they were: the feedback maths and the similarity threshold in
//! [`crate::vitality`], and what makes a memory due for review in
//! [`crate::recalibrate`], which passes its thresholds in.

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineLock};
use super::{Result, Store};
use crate::db::derived::{write_memory, Origin};
use crate::models::RecalibrateCandidate;
use rusqlite::{params, params_from_iter, Connection};

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

/// The feedback tables, over one connection, or on the engine's memories
/// core when the store's tables hold it (`db::engine::feedback`).
pub struct Feedback<'c> {
    conn: &'c Connection,
    #[cfg(feature = "engine-store")]
    core: Option<&'c EngineLock>,
}

impl<'c> Feedback<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            #[cfg(feature = "engine-store")]
            core: store.core(),
        }
    }

    /// `memory_id`'s importance columns, or `None` if it is missing or
    /// deleted.
    pub fn importance(&self, memory_id: &str) -> Result<Option<Importance>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::memories::importance(&core.lock(), memory_id);
        }
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
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::memories::set_importance(
                &mut core.lock(),
                memory_id,
                base_weight,
                vitality,
                status,
            );
        }
        write_memory(self.conn, memory_id, Origin::Local, || {
            Ok(self.conn.execute(
                "UPDATE memories SET base_weight = ?, vitality = ?, status = ? WHERE id = ?",
                params![base_weight, vitality, status, memory_id],
            )?)
        })?;
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
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::feedback::log_event(
                &mut core.lock(),
                id,
                memory_id,
                query,
                event,
                created_at,
            );
        }
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

    /// Every feedback event logged for `memory_id`, oldest first (ties by
    /// id).
    pub fn events(&self, memory_id: &str) -> Result<Vec<FeedbackEvent>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::feedback::events(&core.lock(), memory_id);
        }
        let mut stmt = self.conn.prepare(
            "SELECT query_tokens, signal, magnitude FROM memory_feedback WHERE memory_id = ?
              ORDER BY created_at, id",
        )?;
        let rows = stmt
            .query_map([memory_id], |row| {
                Ok(FeedbackEvent {
                    query_tokens: row.get(0)?,
                    signal: row.get(1)?,
                    magnitude: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Remove every feedback event logged for `memory_id`: part of deleting
    /// a memory, since the table has no foreign key to cascade.
    pub fn delete_for(&self, memory_id: &str) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::feedback::delete_for(&mut core.lock(), memory_id);
        }
        self.conn.execute(
            "DELETE FROM memory_feedback WHERE memory_id = ?",
            params![memory_id],
        )?;
        Ok(())
    }

    /// How many memories `filter` makes due for review.
    pub fn review_count(&self, filter: &ReviewFilter<'_>) -> Result<i64> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::feedback::review_count(&core.lock(), filter);
        }
        let (predicate, bindings) = review_where(filter);
        Ok(self.conn.query_row(
            &format!("SELECT COUNT(*) FROM memories m WHERE {predicate}"),
            params_from_iter(bindings.iter()),
            |r| r.get(0),
        )?)
    }

    /// Up to `limit` memories `filter` makes due for review, heaviest and
    /// longest-unread first (ties by id), their content cut to
    /// `snippet_chars`.
    pub fn review_batch(
        &self,
        filter: &ReviewFilter<'_>,
        snippet_chars: usize,
        limit: i64,
    ) -> Result<Vec<RecalibrateCandidate>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::feedback::review_batch(
                &core.lock(),
                filter,
                snippet_chars,
                usize::try_from(limit).unwrap_or(0),
            );
        }
        let (predicate, mut bindings) = review_where(filter);
        bindings.push(rusqlite::types::Value::Integer(limit));
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, substr(content, 1, {snippet_chars}) AS content_snippet, category,
                    memory_type, base_weight, access_count, accessed_at, created_at
               FROM memories m
              WHERE {predicate}
              ORDER BY m.base_weight DESC, m.accessed_at ASC, m.id
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
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
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
    use crate::db::history::{Revisions, Tracked};
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::{on_each_backend, Database};

    /// Feedback, importance and revert writes on `db`, and every read after.
    fn exercise(db: &Database) -> Vec<String> {
        let old = "2020-01-01T00:00:00+00:00";
        let store = db.store();
        let memories = Memories::new(&store);
        for (id, weight, kind) in [
            ("heavy", 2.0, "note"),
            ("heavier", 3.0, "note"),
            ("fact", 0.5, "fact"),
            ("light", 0.5, "note"),
            ("fresh", 5.0, "note"),
        ] {
            let at = if id == "fresh" {
                "2999-01-01T00:00:00+00:00"
            } else {
                old
            };
            memories
                .insert(&NewMemory {
                    base_weight: weight,
                    memory_type: kind.into(),
                    accessed_at: (id == "heavy").then(|| old.to_string()),
                    ..NewMemory::new(id, format!("content of {id} {}", "ü".repeat(20)), at)
                })
                .unwrap();
        }
        let feedback = Feedback::new(&store);
        let filter = ReviewFilter {
            min_base_weight: 1.0,
            durable_types: &["fact"],
            stale_days: 30,
        };
        let mut seen = Vec::new();
        let look = |feedback: &Feedback<'_>| -> String {
            let batch: Vec<String> = feedback
                .review_batch(&filter, 12, 10)
                .unwrap()
                .into_iter()
                .map(|c| format!("{}:{}:{:?}", c.id, c.content_snippet, c.accessed_at))
                .collect();
            format!("{} {batch:?}", feedback.review_count(&filter).unwrap())
        };
        seen.push(look(&feedback));
        let event = |signal: &str, magnitude| FeedbackEvent {
            query_tokens: "q".into(),
            signal: signal.into(),
            magnitude,
        };
        feedback
            .log_event("fb_2", "heavy", "q", &event("unhelpful", 0.2), old)
            .unwrap();
        feedback
            .log_event("fb_1", "heavy", "q", &event("helpful", 0.1), old)
            .unwrap();
        seen.push(format!(
            "{:?} {:?}",
            feedback
                .log_event("fb_1", "fact", "q", &event("helpful", 0.1), old)
                .is_err(),
            feedback
                .log_event("fb_3", "fact", "q", &event("meh", 0.1), old)
                .is_err(),
        ));
        seen.push(format!("{:?}", feedback.events("heavy").unwrap()));
        seen.push(look(&feedback));
        feedback.set_importance("fact", 4.0, 0.9, "active").unwrap();
        seen.push(format!("{:?}", feedback.importance("fact").unwrap()));
        seen.push(format!("{:?}", feedback.importance("missing").unwrap()));
        seen.push(look(&feedback));
        feedback.delete_for("heavy").unwrap();
        seen.push(format!("{:?}", feedback.events("heavy").unwrap()));
        seen.push(look(&feedback));

        let revisions = Revisions::new(&store);
        seen.push(format!("{:?}", revisions.current("light").unwrap()));
        revisions
            .restore(
                "light",
                &Tracked {
                    content: "restored".into(),
                    category: "c".into(),
                    tags: "not json".into(),
                    metadata: "{}".into(),
                    sensitive: None,
                },
                "2026-09-27T00:00:00+00:00",
            )
            .unwrap();
        seen.push(format!("{:?}", revisions.current("light").unwrap()));
        memories.delete_live("light", Some(old)).unwrap();
        seen.push(format!(
            "{} {} {:?}",
            revisions.is_live("light").unwrap(),
            revisions.is_live("fact").unwrap(),
            revisions.current("light").unwrap().map(|t| t.content),
        ));
        seen
    }

    #[test]
    fn the_engine_core_keeps_feedback_and_history_as_sqlite_does() {
        let mut observed = Vec::new();
        on_each_backend(|db| observed.push(exercise(db)));
        let sqlite = &observed[0];
        assert!(sqlite[0].starts_with("3 "), "{}", sqlite[0]);
        for other in &observed[1..] {
            assert_eq!(other, sqlite);
        }
    }

    #[test]
    fn review_needs_weight_or_a_durable_type_and_no_feedback() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let old = "2020-01-01T00:00:00+00:00";
        for (id, weight, kind) in [
            ("heavy", 2.0, "note"),
            ("fact", 0.5, "fact"),
            ("light", 0.5, "note"),
        ] {
            crate::db::memories::Memories::new(&store)
                .insert(&crate::db::memories::NewMemory {
                    base_weight: weight,
                    memory_type: kind.to_string(),
                    ..crate::db::memories::NewMemory::new(id, format!("content {id}"), old)
                })
                .unwrap();
        }
        let feedback = Feedback::new(&store);
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
