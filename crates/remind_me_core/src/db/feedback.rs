//! Storage for feedback: the `memory_feedback` event log, the importance
//! columns a global judgement rewrites (`base_weight`, `vitality`, `status`),
//! and the recalibration queries that look for memories never given any,
//! on the engine's memories core (`db::engine::feedback`,
//! `db::engine::memories`).
//!
//! Every read and write for feedback goes through here (ADR-0022). The
//! rules stay where they were: the feedback maths and the similarity
//! threshold in [`crate::vitality`], and what makes a memory due for review
//! in [`crate::recalibrate`], which passes its thresholds in.

use super::engine::{self, EngineLock};
use super::{Result, Store};
use crate::models::RecalibrateCandidate;

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

/// The feedback tables, on the engine's memories core.
pub struct Feedback<'c> {
    core: &'c EngineLock,
}

impl<'c> Feedback<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    /// `memory_id`'s importance columns, or `None` if it is missing or
    /// deleted.
    pub fn importance(&self, memory_id: &str) -> Result<Option<Importance>> {
        engine::memories::importance(&self.core.lock(), memory_id)
    }

    /// Rewrite `memory_id`'s importance after a global judgement.
    pub fn set_importance(
        &self,
        memory_id: &str,
        base_weight: f64,
        vitality: f64,
        status: &str,
    ) -> Result<()> {
        engine::memories::set_importance(
            &mut self.core.lock(),
            memory_id,
            base_weight,
            vitality,
            status,
        )
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
        engine::feedback::log_event(
            &mut self.core.lock(),
            id,
            memory_id,
            query,
            event,
            created_at,
        )
    }

    /// Every feedback event logged for `memory_id`, oldest first (ties by
    /// id).
    pub fn events(&self, memory_id: &str) -> Result<Vec<FeedbackEvent>> {
        engine::feedback::events(&self.core.lock(), memory_id)
    }

    /// Remove every feedback event logged for `memory_id`: part of deleting
    /// a memory.
    pub fn delete_for(&self, memory_id: &str) -> Result<()> {
        engine::feedback::delete_for(&mut self.core.lock(), memory_id)
    }

    /// How many memories `filter` makes due for review.
    pub fn review_count(&self, filter: &ReviewFilter<'_>) -> Result<i64> {
        engine::feedback::review_count(&self.core.lock(), filter)
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
        engine::feedback::review_batch(
            &self.core.lock(),
            filter,
            snippet_chars,
            usize::try_from(limit).unwrap_or(0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::history::{Revisions, Tracked};
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    /// Feedback, importance and revert writes, and every read after.
    #[test]
    fn feedback_settles_a_memory_and_history_tracks_its_columns() {
        let old = "2020-01-01T00:00:00+00:00";
        let db = Database::open_in_memory().unwrap();
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
        let look = |feedback: &Feedback<'_>| -> (i64, Vec<String>) {
            let batch: Vec<String> = feedback
                .review_batch(&filter, 12, 10)
                .unwrap()
                .into_iter()
                .map(|c| format!("{}:{}:{:?}", c.id, c.content_snippet, c.accessed_at))
                .collect();
            (feedback.review_count(&filter).unwrap(), batch)
        };
        let (count, batch) = look(&feedback);
        assert_eq!(
            count, 3,
            "heavy, heavier and fact; light is light, fresh is fresh"
        );
        assert_eq!(
            batch,
            [
                "heavier:content of h:None",
                "heavy:content of h:Some(\"2020-01-01T00:00:00+00:00\")",
                "fact:content of f:None",
            ],
            "heaviest first, snippets cut by character"
        );
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
        assert!(
            feedback
                .log_event("fb_1", "fact", "q", &event("helpful", 0.1), old)
                .is_err(),
            "a taken event id is refused"
        );
        assert!(
            feedback
                .log_event("fb_3", "fact", "q", &event("meh", 0.1), old)
                .is_err(),
            "an unknown signal is refused"
        );
        assert_eq!(
            feedback.events("heavy").unwrap(),
            [event("helpful", 0.1), event("unhelpful", 0.2)],
            "same stamp: by id, not by the order they were logged"
        );
        assert_eq!(look(&feedback).0, 2, "feedback settles a memory");
        feedback.set_importance("fact", 4.0, 0.9, "active").unwrap();
        assert_eq!(
            feedback.importance("fact").unwrap(),
            Some(Importance {
                access_count: 0,
                base_weight: 4.0,
                vitality: 0.9,
            })
        );
        assert_eq!(feedback.importance("missing").unwrap(), None);
        assert_eq!(
            look(&feedback).1[0],
            "fact:content of f:None",
            "heaviest first"
        );
        feedback.delete_for("heavy").unwrap();
        assert!(feedback.events("heavy").unwrap().is_empty());
        assert_eq!(
            look(&feedback).0,
            3,
            "with its feedback gone, heavy is due again"
        );

        let revisions = Revisions::new(&store);
        let before = revisions.current("light").unwrap().unwrap();
        assert_eq!(before.category, "general");
        assert_eq!(before.sensitive, Some(false));
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
        let after = revisions.current("light").unwrap().unwrap();
        assert_eq!(after.content, "restored");
        assert_eq!(after.category, "c");
        assert_eq!(
            after.sensitive,
            Some(false),
            "None is written as not sensitive"
        );
        memories.delete_live("light", Some(old)).unwrap();
        assert!(!revisions.is_live("light").unwrap());
        assert!(revisions.is_live("fact").unwrap());
        assert_eq!(
            revisions.current("light").unwrap().map(|t| t.content),
            Some(crate::sync::TOMBSTONE_CONTENT.to_string()),
            "a tombstone's text is gone, but the row still reads"
        );
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
            Memories::new(&store)
                .insert(&NewMemory {
                    base_weight: weight,
                    memory_type: kind.to_string(),
                    ..NewMemory::new(id, format!("content {id}"), old)
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
