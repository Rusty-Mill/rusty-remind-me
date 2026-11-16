//! Storage for feedback: the `memory_feedback` event log, the importance
//! columns a global judgement rewrites (`base_weight`, `vitality`, `status`),
//! and the recalibration queries that look for memories never given any.
//!
//! [`FeedbackStore`] is the port (ADR-0035); the engine's adapter is in
//! `db::engine::feedback`. [`Feedback`] is what callers take: it locks the
//! store's tables for each call and goes through the port. The rules stay
//! where they were: the feedback maths and the similarity
//! threshold in [`crate::vitality`], and what makes a memory due for review
//! in [`crate::recalibrate`], which passes its thresholds in.

use super::engine::EngineLock;
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

/// The feedback log and the importance columns, as a backend stores them
/// (ADR-0035). [`conformance`] holds the behaviour every implementation
/// must show.
pub trait FeedbackStore {
    /// `memory_id`'s importance columns, or `None` if it is missing or
    /// deleted.
    fn importance(&self, memory_id: &str) -> Result<Option<Importance>>;

    /// Rewrite `memory_id`'s importance after a global judgement, without
    /// stamping `updated_at`.
    fn set_importance(
        &mut self,
        memory_id: &str,
        base_weight: f64,
        vitality: f64,
        status: &str,
    ) -> Result<()>;

    /// Log a query-contextual feedback event under `id`. A taken id, and a
    /// signal other than `helpful` or `unhelpful`, are refused.
    fn log_event(
        &mut self,
        id: &str,
        memory_id: &str,
        query: &str,
        event: &FeedbackEvent,
        created_at: &str,
    ) -> Result<()>;

    /// Every feedback event logged for `memory_id`, oldest first (ties by
    /// id).
    fn events(&self, memory_id: &str) -> Result<Vec<FeedbackEvent>>;

    /// Remove every feedback event logged for `memory_id`.
    fn delete_for(&mut self, memory_id: &str) -> Result<()>;

    /// How many memories `filter` makes due for review: live, not
    /// superseded, weighty or of a durable type, unread for `stale_days`,
    /// and never given feedback.
    fn review_count(&self, filter: &ReviewFilter<'_>) -> Result<i64>;

    /// Up to `limit` memories `filter` makes due for review, heaviest first,
    /// then longest unread (never read first), ties by id, their content cut
    /// to `snippet_chars` characters.
    fn review_batch(
        &self,
        filter: &ReviewFilter<'_>,
        snippet_chars: usize,
        limit: usize,
    ) -> Result<Vec<RecalibrateCandidate>>;
}

/// The feedback tables of a [`Store`], one locked call at a time.
pub struct Feedback<'c> {
    core: &'c EngineLock,
}

impl<'c> Feedback<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    /// See [`FeedbackStore::importance`].
    pub fn importance(&self, memory_id: &str) -> Result<Option<Importance>> {
        FeedbackStore::importance(&*self.core.lock(), memory_id)
    }

    /// See [`FeedbackStore::set_importance`].
    pub fn set_importance(
        &self,
        memory_id: &str,
        base_weight: f64,
        vitality: f64,
        status: &str,
    ) -> Result<()> {
        FeedbackStore::set_importance(
            &mut *self.core.lock(),
            memory_id,
            base_weight,
            vitality,
            status,
        )
    }

    /// See [`FeedbackStore::log_event`].
    pub fn log_event(
        &self,
        id: &str,
        memory_id: &str,
        query: &str,
        event: &FeedbackEvent,
        created_at: &str,
    ) -> Result<()> {
        FeedbackStore::log_event(
            &mut *self.core.lock(),
            id,
            memory_id,
            query,
            event,
            created_at,
        )
    }

    /// See [`FeedbackStore::events`].
    pub fn events(&self, memory_id: &str) -> Result<Vec<FeedbackEvent>> {
        FeedbackStore::events(&*self.core.lock(), memory_id)
    }

    /// See [`FeedbackStore::delete_for`]: part of deleting a memory.
    pub fn delete_for(&self, memory_id: &str) -> Result<()> {
        FeedbackStore::delete_for(&mut *self.core.lock(), memory_id)
    }

    /// See [`FeedbackStore::review_count`].
    pub fn review_count(&self, filter: &ReviewFilter<'_>) -> Result<i64> {
        FeedbackStore::review_count(&*self.core.lock(), filter)
    }

    /// See [`FeedbackStore::review_batch`]. A negative `limit` returns
    /// nothing.
    pub fn review_batch(
        &self,
        filter: &ReviewFilter<'_>,
        snippet_chars: usize,
        limit: i64,
    ) -> Result<Vec<RecalibrateCandidate>> {
        let limit = usize::try_from(limit).unwrap_or(0);
        FeedbackStore::review_batch(&*self.core.lock(), filter, snippet_chars, limit)
    }
}

/// The behaviour every [`FeedbackStore`] must show, run against each backend
/// by [`feedback_conformance`] (ADR-0035).
///
/// Feedback hangs off memories, which have no port yet, so each check takes
/// the backend's own way to add a memory (`add`).
#[cfg(test)]
pub(crate) mod conformance {
    use super::{FeedbackEvent, FeedbackStore, Importance, ReviewFilter};
    use crate::db::memories::NewMemory;

    const OLD: &str = "2020-01-01T00:00:00+00:00";

    fn filter() -> ReviewFilter<'static> {
        ReviewFilter {
            min_base_weight: 1.0,
            durable_types: &["fact"],
            stale_days: 30,
        }
    }

    fn event(signal: &str, magnitude: f64) -> FeedbackEvent {
        FeedbackEvent {
            query_tokens: "q".into(),
            signal: signal.into(),
            magnitude,
        }
    }

    fn look<S: FeedbackStore>(store: &S) -> (i64, Vec<String>) {
        let batch: Vec<String> = store
            .review_batch(&filter(), 12, 10)
            .unwrap()
            .into_iter()
            .map(|c| format!("{}:{}:{:?}", c.id, c.content_snippet, c.accessed_at))
            .collect();
        (store.review_count(&filter()).unwrap(), batch)
    }

    /// Feedback and importance writes, and the review reads after each.
    pub(crate) fn feedback_settles_a_memory<S: FeedbackStore>(
        store: &mut S,
        add: impl Fn(&mut S, &NewMemory),
    ) {
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
                OLD
            };
            let new = NewMemory {
                base_weight: weight,
                memory_type: kind.into(),
                accessed_at: (id == "heavy").then(|| OLD.to_string()),
                ..NewMemory::new(id, format!("content of {id} {}", "ü".repeat(20)), at)
            };
            add(store, &new);
        }
        let (count, batch) = look(store);
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
        assert!(store.review_batch(&filter(), 12, 0).unwrap().is_empty());

        store
            .log_event("fb_2", "heavy", "q", &event("unhelpful", 0.2), OLD)
            .unwrap();
        store
            .log_event("fb_1", "heavy", "q", &event("helpful", 0.1), OLD)
            .unwrap();
        assert!(
            store
                .log_event("fb_1", "fact", "q", &event("helpful", 0.1), OLD)
                .is_err(),
            "a taken event id is refused"
        );
        assert!(
            store
                .log_event("fb_3", "fact", "q", &event("meh", 0.1), OLD)
                .is_err(),
            "an unknown signal is refused"
        );
        assert_eq!(
            store.events("heavy").unwrap(),
            [event("helpful", 0.1), event("unhelpful", 0.2)],
            "same stamp: by id, not by the order they were logged"
        );
        assert_eq!(look(store).0, 2, "feedback settles a memory");

        store.set_importance("fact", 4.0, 0.9, "active").unwrap();
        assert_eq!(
            store.importance("fact").unwrap(),
            Some(Importance {
                access_count: 0,
                base_weight: 4.0,
                vitality: 0.9,
            })
        );
        assert_eq!(store.importance("missing").unwrap(), None);
        assert_eq!(look(store).1[0], "fact:content of f:None", "heaviest first");

        store.delete_for("heavy").unwrap();
        assert!(store.events("heavy").unwrap().is_empty());
        assert_eq!(
            look(store).0,
            3,
            "with its feedback gone, heavy is due again"
        );
    }

    pub(crate) fn review_needs_weight_or_a_durable_type<S: FeedbackStore>(
        store: &mut S,
        add: impl Fn(&mut S, &NewMemory),
    ) {
        for (id, weight, kind) in [
            ("heavy", 2.0, "note"),
            ("fact", 0.5, "fact"),
            ("light", 0.5, "note"),
        ] {
            let new = NewMemory {
                base_weight: weight,
                memory_type: kind.to_string(),
                ..NewMemory::new(id, format!("content {id}"), OLD)
            };
            add(store, &new);
        }
        assert_eq!(store.review_count(&filter()).unwrap(), 2);
        let no_types = ReviewFilter {
            durable_types: &[],
            ..filter()
        };
        assert_eq!(
            store.review_count(&no_types).unwrap(),
            1,
            "without durable types only weight counts"
        );
        let none = ReviewFilter {
            min_base_weight: 10.0,
            durable_types: &[],
            ..filter()
        };
        assert_eq!(store.review_count(&none).unwrap(), 0);
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`, with `$add` as the checks' memory fixture.
#[cfg(test)]
macro_rules! feedback_conformance {
    ($make:expr, $add:expr) => {
        use $crate::db::feedback::conformance;

        #[test]
        fn feedback_settles_a_memory() {
            conformance::feedback_settles_a_memory(&mut $make, $add);
        }
        #[test]
        fn review_needs_weight_or_a_durable_type() {
            conformance::review_needs_weight_or_a_durable_type(&mut $make, $add);
        }
    };
}
#[cfg(test)]
pub(crate) use feedback_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    #[test]
    fn the_repository_reaches_the_store_through_the_port() {
        let old = "2020-01-01T00:00:00+00:00";
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        Memories::new(&store)
            .insert(&NewMemory {
                base_weight: 2.0,
                ..NewMemory::new("m", "content", old)
            })
            .unwrap();
        let feedback = Feedback::new(&store);
        let filter = ReviewFilter {
            min_base_weight: 1.0,
            durable_types: &[],
            stale_days: 30,
        };
        assert_eq!(feedback.review_count(&filter).unwrap(), 1);
        assert_eq!(
            feedback.review_batch(&filter, 3, 10).unwrap()[0].content_snippet,
            "con"
        );
        assert!(
            feedback.review_batch(&filter, 3, -1).unwrap().is_empty(),
            "a negative limit returns nothing"
        );
        let event = FeedbackEvent {
            query_tokens: "q".into(),
            signal: "helpful".into(),
            magnitude: 0.1,
        };
        feedback.log_event("fb_1", "m", "q", &event, old).unwrap();
        assert_eq!(feedback.events("m").unwrap(), [event]);
        feedback.set_importance("m", 3.0, 0.5, "active").unwrap();
        assert_eq!(
            feedback.importance("m").unwrap().map(|i| i.base_weight),
            Some(3.0)
        );
        feedback.delete_for("m").unwrap();
        assert!(feedback.events("m").unwrap().is_empty());
    }
}
