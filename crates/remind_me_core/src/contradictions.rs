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
//!
//! # On save, for decisions
//!
//! The queue above only runs when asked, so a stale decision can sit beside
//! its replacement indefinitely. [`check_on_save`] runs a narrow version of
//! it when a decision is written: only against standing decisions, only
//! those sharing the claim or a non-hub entity, and a shared entity alone is
//! not enough — the content must also overlap by [`ON_SAVE_MIN_OVERLAP`], so
//! two decisions that merely name the same project are not flagged. At most
//! [`ON_SAVE_LIMIT`] come back with the write. It never blocks the write and
//! never supersedes: the writer decides, and can do it in the same call with
//! `remind_me_add`'s `supersedes`.

use crate::db::curation::Curation;
use crate::db::memories::Memories;
use crate::db::Result;
use crate::db::Store;
use crate::kinds::StructuredFields;
use crate::models::{
    ConflictReason, ContradictionCandidate, ContradictionCandidatesResult, ContradictionSide,
    DecisionConflict, Memory, MemoryUpdateInput, SaveCheck,
};
use crate::vitality::{jaccard, tokenize_query};
use std::cmp::Ordering;
use std::collections::HashSet;

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

fn side(store: &Store<'_>, memory_id: &str) -> Result<ContradictionSide> {
    Curation::new(store).contradiction_side(memory_id, SNIPPET_CHARS)
}

/// Whether two memories state the same subject and predicate with different
/// objects (case and surrounding space ignored): a conflict worth ranking,
/// not merely two memories on one topic.
fn conflicting_triples(a: &Memory, b: &Memory) -> bool {
    let norm = |s: &Option<String>| s.as_deref().map(|s| s.trim().to_lowercase());
    let (sa, sb) = (norm(&a.subject), norm(&b.subject));
    let (pa, pb) = (norm(&a.predicate), norm(&b.predicate));
    let (oa, ob) = (norm(&a.object), norm(&b.object));
    sa.is_some() && sa == sb && pa.is_some() && pa == pb && oa.is_some() && ob.is_some() && oa != ob
}

/// Which of two conflicting memories to keep: higher confidence, then newer
/// `verified_at`, then newer `updated_at`. `None` for a pair that is not a
/// triple conflict, or ties on all three. RFC 3339 strings are compared as
/// text, as everywhere else in this crate.
pub fn preferred<'a>(a: &'a Memory, b: &'a Memory) -> Option<&'a Memory> {
    if !conflicting_triples(a, b) {
        return None;
    }
    let order = a
        .confidence
        .partial_cmp(&b.confidence)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.verified_at.cmp(&b.verified_at))
        .then_with(|| a.updated_at.cmp(&b.updated_at));
    match order {
        Ordering::Greater => Some(a),
        Ordering::Less => Some(b),
        Ordering::Equal => None,
    }
}

/// A batch of candidate pairs, plus the full backlog size.
/// How many candidate pairs there are, without materialising any of them.
///
/// Reuses [`pairs_sql`] rather than approximating with a second query, so the
/// maintenance nudge cannot claim a backlog that draining it does not find.
pub fn candidate_count(store: &Store<'_>) -> Result<i64> {
    Curation::new(store).count_contradiction_pairs(MAX_ENTITY_FANOUT)
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
fn shared_entities(store: &Store<'_>, id_a: &str, id_b: &str) -> Result<Vec<String>> {
    Curation::new(store).shared_entity_names(id_a, id_b)
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
    store: &Store<'_>,
    limit: usize,
    cursor: Option<(&str, &str)>,
) -> Result<ContradictionCandidatesResult> {
    let curation = Curation::new(store);

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
    let memories = Memories::new(store);
    for (id_a, id_b) in ids {
        // Only the sides swap; the cursor above came from `ids` and is
        // unaffected.
        let keep = match (memories.get_live(&id_a)?, memories.get_live(&id_b)?) {
            (Some(a), Some(b)) => preferred(&a, &b).map(|m| m.id.clone()),
            _ => None,
        };
        let (first, second) = match &keep {
            Some(k) if *k == id_b => (&id_b, &id_a),
            _ => (&id_a, &id_b),
        };
        candidates.push(ContradictionCandidate {
            shared_entities: shared_entities(store, &id_a, &id_b)?,
            memory_a: side(store, first)?,
            memory_b: side(store, second)?,
            recommended_keep: keep,
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

/// At most this many conflicts come back with a saved decision.
pub const ON_SAVE_LIMIT: usize = 5;

/// The word overlap (Jaccard) a decision sharing only an entity needs to be
/// returned: the threshold feedback matching already uses for "the same
/// topic" ([`crate::vitality::FEEDBACK_SIMILARITY_THRESHOLD`]).
pub const ON_SAVE_MIN_OVERLAP: f64 = crate::vitality::FEEDBACK_SIMILARITY_THRESHOLD;

/// Whether two memories state the same subject and predicate with the same
/// object: a restatement, which is no conflict.
fn same_triple(a: &Memory, b: &Memory) -> bool {
    let norm = |s: &Option<String>| s.as_deref().map(|s| s.trim().to_lowercase());
    norm(&a.subject).is_some()
        && norm(&a.subject) == norm(&b.subject)
        && norm(&a.predicate) == norm(&b.predicate)
        && norm(&a.object) == norm(&b.object)
}

fn token_set(tokens: &[String]) -> HashSet<&str> {
    tokens.iter().map(String::as_str).collect()
}

/// Word overlap (Jaccard) of two texts, tokenized as search queries are.
fn overlap(a: &str, b: &str) -> f64 {
    let (ta, tb) = (tokenize_query(a), tokenize_query(b));
    jaccard(&token_set(&ta), &token_set(&tb))
}

/// How `other` conflicts with `saved`, if it does.
fn conflict(saved: &Memory, other: &Memory, shared: &[String]) -> Option<(f64, ConflictReason)> {
    if conflicting_triples(saved, other) {
        return Some((1.0, ConflictReason::SameClaim));
    }
    if shared.is_empty() || same_triple(saved, other) {
        return None;
    }
    let score = overlap(&saved.content, &other.content);
    (score >= ON_SAVE_MIN_OVERLAP).then_some((score, ConflictReason::SharedEntity))
}

/// The standing decisions that `memory_id`, if it is a live decision, may
/// contradict: highest score first, then newest, at most [`ON_SAVE_LIMIT`].
/// Empty for any other memory.
pub fn decision_conflicts(store: &Store<'_>, memory_id: &str) -> Result<Vec<DecisionConflict>> {
    let memories = Memories::new(store);
    let Some(saved) = memories.get_live(memory_id)? else {
        return Ok(Vec::new());
    };
    if saved.memory_type.as_deref() != Some("decision") || saved.superseded_by.is_some() {
        return Ok(Vec::new());
    }
    let curation = Curation::new(store);
    let mut found = Vec::new();
    for (id, shared) in curation.decision_neighbours(memory_id, MAX_ENTITY_FANOUT)? {
        let Some(other) = memories.get_live(&id)? else {
            continue;
        };
        if let Some((score, reason)) = conflict(&saved, &other, &shared) {
            found.push((other.created_at.clone(), score, reason, shared, id));
        }
    }
    found.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.0.cmp(&a.0))
    });
    found
        .into_iter()
        .take(ON_SAVE_LIMIT)
        .map(|(_, score, reason, shared_entities, id)| {
            Ok(DecisionConflict {
                memory: side(store, &id)?,
                score,
                reason,
                shared_entities,
            })
        })
        .collect()
}

/// A check's outcome as a write reports it. A failure is logged with the
/// memory it concerns and carried in the result, never dropped: the write
/// it followed has already committed and stands.
pub fn save_check(memory_id: &str, outcome: Result<Vec<DecisionConflict>>) -> SaveCheck {
    match outcome {
        Ok(contradiction_candidates) => SaveCheck {
            contradiction_candidates,
            contradiction_check_error: None,
        },
        Err(e) => {
            eprintln!("rusty-remind-me: contradiction check on save of {memory_id} failed: {e}");
            SaveCheck {
                contradiction_candidates: Vec::new(),
                contradiction_check_error: Some(e.to_string()),
            }
        }
    }
}

/// Run the contradiction check for a just-written memory. Never fails: see
/// [`save_check`].
pub fn check_on_save(store: &Store<'_>, memory_id: &str) -> SaveCheck {
    save_check(memory_id, decision_conflicts(store, memory_id))
}

/// [`check_on_save`] after an update, run only when the edit could change the
/// answer: new content or a new type. A tag or metadata edit reports nothing.
pub fn check_on_update(
    store: &Store<'_>,
    input: &MemoryUpdateInput,
    fields: &StructuredFields,
) -> SaveCheck {
    if input.content.is_none() && fields.memory_type.is_none() {
        return SaveCheck::default();
    }
    check_on_save(store, &input.memory_id)
}
