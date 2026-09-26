//! Free-text contradiction candidates.
//!
//! [`crate::entity`]'s write path already auto-supersedes a memory whenever a
//! new triple shares an existing one's (subject, predicate) but asserts a
//! different object. That mechanism only fires on exact triple structure. It
//! says nothing about two pieces of prose that conflict without either
//! carrying a formal triple — "I moved to Boston" against "I live in Seattle",
//! written as plain text. This surfaces exactly that gap.
//!
//! Read-only, and it returns pairs that *might* conflict rather than pairs
//! that do. Prose comparison is inherently less certain than exact triple
//! matching, so the judgment stays with the calling session; most pairs turn
//! out merely topically similar. There is deliberately no apply tool — a
//! confirmed contradiction is fixed with the existing `remind_me_update`,
//! `remind_me_delete`, or an `remind_me_add` carrying an explicit triple.
//!
//! # Why the fan-out cap exists
//!
//! The comparison space is bounded by the entity graph rather than all-pairs:
//! two memories are only worth comparing if they mention an entity in common.
//! That alone is not enough. A broadly-mentioned entity — a person, or a
//! project with hundreds of memories about it — makes "shares an entity" stop
//! meaning anything, and the pair count from one such entity is quadratic in
//! its mention count.
//!
//! The reference measured this on a real vault: a single 745-mention project
//! entity produced 277,140 of 372,750 total candidates — 74% of the queue —
//! before the cap existed. Entities mentioned by more than
//! [`MAX_ENTITY_FANOUT`] memories are excluded from the join entirely, on both
//! sides, since either side of the self-join can land on the hub entity.
//!
//! This is not an optimisation to add later. Without it the queue is
//! dominated by pairs whose only relationship is naming the same project, and
//! the tool is unusable on exactly the vaults that need it most.

use crate::db::curation::Curation;
use crate::models::{ContradictionCandidate, ContradictionCandidatesResult, ContradictionSide};
use rusqlite::{Connection, Result};

/// Entities mentioned by more memories than this are excluded from the pairing
/// join.
///
/// Chosen empirically by the reference as the fan-out above which pairs stop
/// reading as plausible candidates and start reading as "these two both
/// mention the same project".
pub const MAX_ENTITY_FANOUT: i64 = 20;

/// Characters of each side's content returned, enough to judge a pair without
/// returning two whole documents per row.
const SNIPPET_CHARS: usize = 500;

fn side(conn: &Connection, memory_id: &str) -> Result<ContradictionSide> {
    Curation::new(conn).contradiction_side(memory_id, SNIPPET_CHARS)
}

/// A batch of candidate pairs, plus the full backlog size.
/// How many candidate pairs there are, without materialising any of them.
///
/// Reuses [`pairs_sql`] rather than approximating with a second query, so the
/// maintenance nudge cannot claim a backlog that draining it does not find.
pub fn candidate_count(conn: &Connection) -> Result<i64> {
    Curation::new(conn).count_contradiction_pairs(MAX_ENTITY_FANOUT)
}

/// The entity names both memories mention, in name order.
///
/// `DISTINCT` because an entity can be linked to the same memory more than
/// once through different mention rows, and the caller wants the set of shared
/// entities rather than a mention count.
///
/// Ordered by name so two calls return the same list in the same order — the
/// pair itself is stable across pages, and a field that reshuffled between
/// identical requests would make responses gratuitously un-diffable.
fn shared_entities(conn: &Connection, id_a: &str, id_b: &str) -> Result<Vec<String>> {
    Curation::new(conn).shared_entity_names(id_a, id_b)
}

/// A page of candidate pairs, optionally starting after `cursor`.
///
/// # Why a keyset and not an `OFFSET`
///
/// The pair set is *derived* from live memories, so a memory edited or deleted
/// between two calls changes it. An offset would then silently skip or repeat
/// rows around the edit. A keyset asks "after this pair" and is stable under
/// both.
///
/// Without any cursor every call re-served the identical first page, so a
/// queue of tens of thousands of pairs had exactly `limit` reachable rows and
/// a worker looping this tool made no progress at all.
///
/// Of the three approaches the reference's issue floated, this is the only one
/// that keeps the module read-only. Excluding already-reviewed pairs needs
/// somewhere to record a review, and there is deliberately no apply/resolve
/// tool here; ordering by `updated_at` fails outright, since most pairs are
/// correctly judged *not* to conflict, so reviewing them changes nothing and
/// they would be re-served forever — the very bug being fixed.
pub fn candidates(
    conn: &Connection,
    limit: usize,
    cursor: Option<(&str, &str)>,
) -> Result<ContradictionCandidatesResult> {
    let curation = Curation::new(conn);

    // The whole queue, deliberately not narrowed by the cursor: a caller
    // watching this number shrink as it pages would be watching the backlog
    // it has left to review, which is not what "how big is the backlog" means
    // to the maintenance nudge that also reads it.
    let total = curation.count_contradiction_pairs(MAX_ENTITY_FANOUT)?;
    let ids = curation.contradiction_pairs(MAX_ENTITY_FANOUT, cursor, limit)?;

    // Null on a short page, which is the queue being exhausted. A full page
    // whose last row happens to be the final one costs the caller one extra
    // request returning nothing, which is the standard keyset trade and beats
    // over-reading by a row on every page.
    let next = if ids.len() == limit { ids.last() } else { None };
    let (next_after_a, next_after_b) = match next {
        Some((a, b)) => (Some(a.clone()), Some(b.clone())),
        None => (None, None),
    };

    let mut candidates = Vec::with_capacity(ids.len());
    for (id_a, id_b) in ids {
        candidates.push(ContradictionCandidate {
            shared_entities: shared_entities(conn, &id_a, &id_b)?,
            memory_a: side(conn, &id_a)?,
            memory_b: side(conn, &id_b)?,
        });
    }

    Ok(ContradictionCandidatesResult {
        candidates,
        total_candidates: total,
        has_more: next_after_a.is_some(),
        next_after_a,
        next_after_b,
    })
}
