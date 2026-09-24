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

use crate::models::{
    RecalibrateCandidate, RecalibrateCandidatesInput, RecalibrateCandidatesResult,
};
use rusqlite::{params, Connection, Result};

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

/// Which memories look important, have gone quiet, and have never been
/// reviewed.
///
/// The three clauses are independent and all required. `base_weight` OR a
/// durable `memory_type` covers importance from either direction — a memory can
/// be born important by type without its weight ever moving. The absence of any
/// `memory_feedback` row stands in for "never actually reviewed", which is the
/// reference's own stated proxy rather than a claim that feedback is the only
/// form review takes.
fn candidate_where() -> String {
    let types = RECALIBRATION_DURABLE_TYPES
        .iter()
        .map(|t| format!("'{}'", t))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "m.superseded_by IS NULL
         AND m.deleted_at IS NULL
         AND (
             m.base_weight >= {min_weight}
             OR m.memory_type IN ({types})
         )
         AND (julianday('now') - julianday(COALESCE(m.accessed_at, m.created_at))) >= {stale_days}
         AND NOT EXISTS (
             SELECT 1 FROM memory_feedback mf WHERE mf.memory_id = m.id
         )",
        min_weight = RECALIBRATION_MIN_BASE_WEIGHT,
        types = types,
        stale_days = RECALIBRATION_STALE_DAYS
    )
}

/// A batch of memories whose importance classification may be stale, plus the
/// full backlog size.
///
/// `total_candidates` is counted rather than derived from the returned batch:
/// the point of the number is to tell the caller how much is left behind the
/// `limit`, so it has to come from the same predicate without it.
/// How many memories are due an importance review, without materialising them.
///
/// Reuses [`candidate_where`] for the same reason as the contradiction count:
/// a nudge that disagrees with the tool it points at trains the reader to
/// ignore it.
pub fn candidate_count(conn: &Connection) -> Result<i64> {
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM memories m WHERE {}",
            candidate_where()
        ),
        [],
        |r| r.get(0),
    )
}

pub fn candidates(
    conn: &Connection,
    input: &RecalibrateCandidatesInput,
) -> Result<RecalibrateCandidatesResult> {
    let predicate = candidate_where();

    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM memories m WHERE {}", predicate),
        [],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(&format!(
        "SELECT id, substr(content, 1, {snippet}) AS content_snippet, category,
                memory_type, base_weight, access_count, accessed_at, created_at
           FROM memories m
          WHERE {predicate}
          ORDER BY m.base_weight DESC, m.accessed_at ASC
          LIMIT ?",
        snippet = SNIPPET_CHARS,
        predicate = predicate
    ))?;

    // rusqlite 0.32+ has no `ToSql` for `usize`; a limit past `i64::MAX` is
    // unbounded either way, so saturating is exact rather than a truncation.
    let limit = i64::try_from(input.limit).unwrap_or(i64::MAX);
    let candidates = stmt
        .query_map(params![limit], |r| {
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
        .collect::<Result<Vec<_>>>()?;

    Ok(RecalibrateCandidatesResult {
        candidates,
        total_candidates: total,
    })
}
