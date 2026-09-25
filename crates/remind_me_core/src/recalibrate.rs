//! Importance-recalibration candidates.
//!
//! [`crate::vitality`] seeds `base_weight` at write time from `memory_type` and
//! `source` — an importance *prior* — and adjusts it from explicit
//! `remind_me_feedback` signals. Nothing re-examines whether that original
//! classification has since gone stale: a "decision" later reversed by a
//! different memory, or a "fact" superseded in spirit but never through the
//! formal triple-supersession path, both keep the importance they were born
//! with.
//!
//! This is the surfacing half of the same two-phase, client-side-judgment shape
//! [`crate::normalize`] uses: a deterministic heuristic narrows an unbounded set
//! to a reviewable batch, and the calling session decides whether any given
//! memory is actually misclassified. The reference is explicit that this is a
//! deliberate departure from its own issue text, which proposed an LLM-driven
//! background pass — neither codebase has an in-server model to call.
//!
//! There is deliberately **no apply half**. The write path already exists twice
//! over: `remind_me_reclassify`/`_batch` change `memory_type` (and the
//! `decay_rate` that follows from it), and `remind_me_feedback` nudges
//! `base_weight` alone when the type is right but the weight is not. A third
//! writer here would duplicate both.

use crate::db::feedback::{Feedback, ReviewFilter};
use crate::models::{RecalibrateCandidatesInput, RecalibrateCandidatesResult};
use rusqlite::{Connection, Result};

/// `base_weight` floor for the "looks important" half of the heuristic.
///
/// Not an arbitrary cutoff: it is exactly
/// [`crate::vitality::get_type_prior`]'s `fact`/`insight` seed, i.e. the point
/// at which the write-time prior itself already treats a memory as more than
/// default-important.
pub const RECALIBRATION_MIN_BASE_WEIGHT: f64 = 1.15;

/// `memory_type` values whose category implies durability on its own, even when
/// `base_weight` has not been raised by seeding or feedback.
pub const RECALIBRATION_DURABLE_TYPES: [&str; 2] = ["decision", "fact"];

/// Days since last access — or creation, for a memory never accessed — before
/// an important-looking memory is stale enough to be worth a second look.
///
/// A memory still being actively retrieved is presumably still classified
/// correctly, so recent activity disqualifies rather than qualifies.
pub const RECALIBRATION_STALE_DAYS: i64 = 90;

/// Characters of content returned per candidate, enough to judge from without
/// returning whole documents.
const SNIPPET_CHARS: usize = 500;

/// What makes a memory due for review, from the thresholds above. Shared by
/// the count and the batch, so the nudge never disagrees with the tool it
/// points at.
fn review_filter() -> ReviewFilter<'static> {
    ReviewFilter {
        min_base_weight: RECALIBRATION_MIN_BASE_WEIGHT,
        durable_types: &RECALIBRATION_DURABLE_TYPES,
        stale_days: RECALIBRATION_STALE_DAYS,
    }
}

/// How many memories are due an importance review, without materialising them.
///
/// Reuses [`review_filter`] for the same reason as the contradiction count:
/// a nudge that disagrees with the tool it points at trains the reader to
/// ignore it.
pub fn candidate_count(conn: &Connection) -> Result<i64> {
    Feedback::new(conn).review_count(&review_filter())
}

/// A batch of memories whose importance classification may be stale, plus the
/// full backlog size.
///
/// `total_candidates` is counted rather than derived from the returned batch:
/// the point of the number is to tell the caller how much is left behind the
/// `limit`, so it has to come from the same predicate without it.
pub fn candidates(
    conn: &Connection,
    input: &RecalibrateCandidatesInput,
) -> Result<RecalibrateCandidatesResult> {
    let feedback = Feedback::new(conn);
    let filter = review_filter();
    let total = feedback.review_count(&filter)?;
    // rusqlite 0.32+ has no `ToSql` for `usize`; a limit past `i64::MAX` is
    // unbounded either way, so saturating is exact rather than a truncation.
    let limit = i64::try_from(input.limit).unwrap_or(i64::MAX);
    let candidates = feedback.review_batch(&filter, SNIPPET_CHARS, limit)?;

    Ok(RecalibrateCandidatesResult {
        candidates,
        total_candidates: total,
    })
}
