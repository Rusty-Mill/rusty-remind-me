//! Near-duplicate warning on save (Mill Board #1486).
//!
//! Near-duplicates were only found when someone ran `remind_me_consolidate`.
//! This runs a bounded check when a memory is written and returns the likely
//! duplicates with the write, so the writer can merge, supersede or keep it.
//! Like the contradiction check on save ([`crate::contradictions`], #56), it
//! never blocks the write and never merges: the writer decides.
//!
//! # What counts
//!
//! A candidate is a live, unsuperseded, non-sensitive, non-raw memory in the
//! same project as the new one (both unset counts as the same project). It is
//! returned when any of these hold:
//!
//! - **exact**: the same text, ignoring case and spacing;
//! - **keyword**: word overlap (Jaccard, tokenized as
//!   [`crate::contradictions`] does) of at least [`KEYWORD_MIN_OVERLAP`];
//! - **semantic**: cosine similarity of the new memory's first chunk vector
//!   and the candidate's best chunk of at least [`SEMANTIC_MIN_SIMILARITY`].
//!
//! Sensitive memories are left out because the findings ride an ordinary
//! write's response, which must not surface sensitive text unasked.
//!
//! # Cost
//!
//! Two bounded lookups, no model call and never a scan of the store: one
//! full-text query over at most [`MAX_QUERY_TERMS`] of the new text's words
//! returning [`POOL`] hits, and, only when the new memory already has a stored
//! vector, exact similarity against a bounded set of ids: the keyword hits
//! plus what the last built ANN index proposes. Without a built index a
//! paraphrase sharing no word with the new text is missed; that is the price
//! of the bound. Nothing is embedded here: `remind_me_add` embeds before this
//! runs, so the semantic side reuses that vector. A memory with no vector (no
//! embedder, or a fact from `remind_me_decompose`, which does not embed) gets
//! the keyword side only. Budget: well under the write's own embedding round trip, about the
//! cost of one compact `remind_me_search`.

use crate::db::memories::{KeywordFilter, Memories};
use crate::db::Result;
use crate::db::Store;
use crate::models::{
    ContradictionSide, DuplicateCandidate, DuplicateCheck, DuplicateReason, Memory,
};
use crate::vitality::{jaccard, tokenize_query};
use std::collections::{HashMap, HashSet};

/// Word overlap (Jaccard) at or above which two memories read as the same
/// statement. Higher than the contradiction check's "same topic" bar (0.3),
/// because a duplicate says the same thing, not merely the same subject.
pub const KEYWORD_MIN_OVERLAP: f64 = 0.6;

/// Cosine similarity at or above which two embeddings read as the same
/// statement: above `remind_me_consolidate`'s default clustering threshold
/// (0.85), since this warns on every write and should rarely be noise.
pub const SEMANTIC_MIN_SIMILARITY: f32 = 0.9;

/// Candidates fetched from each side before scoring.
pub const POOL: usize = 10;

/// At most this many duplicates come back with a write.
pub const LIMIT: usize = 3;

/// At most this many of the new text's words go into the full-text query, so
/// a long memory does not become a query of hundreds of terms.
pub const MAX_QUERY_TERMS: usize = 32;

/// Characters of each duplicate's content returned with the write.
const SNIPPET_CHARS: usize = 200;

/// The new text's distinct words in order of first appearance, lowercased,
/// single characters dropped, at most [`MAX_QUERY_TERMS`].
fn query_terms(content: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    content
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() > 1)
        .map(str::to_lowercase)
        .filter(|t| seen.insert(t.clone()))
        .take(MAX_QUERY_TERMS)
        .collect()
}

/// Text with case and runs of whitespace normalised away.
fn normalised(text: &str) -> String {
    text.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn token_set(tokens: &[String]) -> HashSet<&str> {
    tokens.iter().map(String::as_str).collect()
}

/// Word overlap of two texts, tokenized as search queries are.
fn overlap(a: &str, b: &str) -> f64 {
    let (ta, tb) = (tokenize_query(a), tokenize_query(b));
    jaccard(&token_set(&ta), &token_set(&tb))
}

fn same_project(a: &Memory, b: &Memory) -> bool {
    match (a.project.as_deref(), b.project.as_deref()) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        (None, None) => true,
        _ => false,
    }
}

/// Whether `other` may be reported as a duplicate of `saved`.
fn admissible(saved: &Memory, other: &Memory) -> bool {
    other.id != saved.id
        && other.superseded_by.is_none()
        && other.deleted_at.is_none()
        && !other.sensitive
        && !crate::db::memories::is_raw_category(&other.category)
        && same_project(saved, other)
}

/// How `other` duplicates `saved`, if it does: the strongest reason and its
/// score. `cosine` is the semantic similarity, when one was measured.
pub fn classify(saved: &str, other: &str, cosine: Option<f32>) -> Option<(f64, DuplicateReason)> {
    if normalised(saved) == normalised(other) {
        return Some((1.0, DuplicateReason::Exact));
    }
    let keyword = overlap(saved, other);
    let semantic = cosine
        .filter(|c| *c >= SEMANTIC_MIN_SIMILARITY)
        .map(f64::from);
    match (keyword >= KEYWORD_MIN_OVERLAP, semantic) {
        (true, Some(s)) if s > keyword => Some((s, DuplicateReason::Semantic)),
        (true, _) => Some((keyword, DuplicateReason::Keyword)),
        (false, Some(s)) => Some((s, DuplicateReason::Semantic)),
        (false, None) => None,
    }
}

fn side_of(memory: &Memory) -> ContradictionSide {
    ContradictionSide {
        id: memory.id.clone(),
        content_snippet: memory.content.chars().take(SNIPPET_CHARS).collect(),
        category: memory.category.clone(),
        memory_type: memory.memory_type.clone(),
        subject: memory.subject.clone(),
        predicate: memory.predicate.clone(),
        object: memory.object.clone(),
        created_at: memory.created_at.clone(),
    }
}

/// Keyword candidates: the full-text hits for the new text's words, in the
/// new memory's project. A projectless memory is narrowed to projectless
/// hits before the limit, since an unset scope would admit every project and
/// let other projects' hits fill the pool.
fn keyword_pool(store: &Store<'_>, saved: &Memory) -> Result<Vec<Memory>> {
    let terms = query_terms(&saved.content);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut filter = KeywordFilter {
        exclude_raw: true,
        ..Default::default()
    };
    filter.scope.project = saved.project.clone();
    filter.narrowing.projectless = saved.project.is_none();
    // One extra, since the new memory itself is the likeliest hit.
    let hits = Memories::new(store).keyword_hits(&terms, &filter, POOL + 1)?;
    Ok(hits
        .into_iter()
        .map(|(memory, _)| memory)
        .filter(|m| admissible(saved, m))
        .collect())
}

/// Semantic candidates, with their similarity: the new memory's stored
/// first-chunk vector scored against a bounded set of ids, the keyword pool's
/// plus what the last built ANN index proposes. The index is always stale
/// here (this write just added a vector), so it is read as of its build; with
/// no index only the keyword pool is scored. Never a scan of every chunk.
/// Empty when the new memory has no vector.
fn semantic_pool(
    store: &Store<'_>,
    saved: &Memory,
    keyword: &[Memory],
) -> Result<Vec<(Memory, f32)>> {
    let Some(vector) = crate::vectors::first_chunk_vector(store, &saved.id)? else {
        return Ok(Vec::new());
    };
    let mut ids: Vec<String> =
        crate::ann_index::stale_candidates(store, &vector, POOL).unwrap_or_default();
    for memory in keyword {
        if !ids.contains(&memory.id) {
            ids.push(memory.id.clone());
        }
    }
    crate::vectors::nearest_among(store, &vector, POOL, &ids, &|m| admissible(saved, m))
}

/// The memories `memory_id` may duplicate: highest score first, then newest,
/// at most [`LIMIT`]. Empty when `memory_id` is not a live memory.
pub fn near_duplicates(store: &Store<'_>, memory_id: &str) -> Result<Vec<DuplicateCandidate>> {
    let Some(saved) = Memories::new(store).get_live(memory_id)? else {
        return Ok(Vec::new());
    };
    let keyword = keyword_pool(store, &saved)?;
    let mut pool: HashMap<String, (Memory, Option<f32>)> = HashMap::new();
    for (memory, cosine) in semantic_pool(store, &saved, &keyword)? {
        pool.insert(memory.id.clone(), (memory, Some(cosine)));
    }
    for memory in keyword {
        pool.entry(memory.id.clone()).or_insert((memory, None));
    }

    let mut found: Vec<(f64, DuplicateReason, Memory)> = pool
        .into_values()
        .filter_map(|(memory, cosine)| {
            classify(&saved.content, &memory.content, cosine)
                .map(|(score, reason)| (score, reason, memory))
        })
        .collect();
    found.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| b.2.created_at.cmp(&a.2.created_at))
            .then_with(|| a.2.id.cmp(&b.2.id))
    });
    Ok(found
        .into_iter()
        .take(LIMIT)
        .map(|(score, reason, memory)| DuplicateCandidate {
            memory: side_of(&memory),
            score,
            reason,
        })
        .collect())
}

/// A check's outcome as a write reports it. A failure is logged with the
/// memory it concerns and carried in the result, never dropped: the write
/// it followed has already committed and stands.
pub fn duplicate_check(
    memory_id: &str,
    outcome: Result<Vec<DuplicateCandidate>>,
) -> DuplicateCheck {
    match outcome {
        Ok(duplicate_candidates) => DuplicateCheck {
            duplicate_candidates,
            duplicate_check_error: None,
        },
        Err(e) => {
            eprintln!("rusty-remind-me: duplicate check on save of {memory_id} failed: {e}");
            DuplicateCheck {
                duplicate_candidates: Vec::new(),
                duplicate_check_error: Some(e.to_string()),
            }
        }
    }
}

/// Run the near-duplicate check for a just-written memory. Never fails: see
/// [`duplicate_check`].
pub fn check_on_save(store: &Store<'_>, memory_id: &str) -> DuplicateCheck {
    duplicate_check(memory_id, near_duplicates(store, memory_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_text_with_other_spacing_and_case_is_exact() {
        let found = classify(
            "Use  Postgres\nfor sessions",
            "use postgres for SESSIONS",
            None,
        );
        assert_eq!(found, Some((1.0, DuplicateReason::Exact)));
    }

    #[test]
    fn high_word_overlap_is_a_keyword_duplicate() {
        let (score, reason) = classify(
            "Nano prefers merge commits over squash merges",
            "Nano prefers merge commits over squash merges always",
            None,
        )
        .unwrap();
        assert_eq!(reason, DuplicateReason::Keyword);
        assert!(score >= KEYWORD_MIN_OVERLAP);
    }

    #[test]
    fn a_close_embedding_is_a_semantic_duplicate() {
        let found = classify("The cat sat", "A feline was seated", Some(0.95));
        assert_eq!(found.map(|f| f.1), Some(DuplicateReason::Semantic));
    }

    #[test]
    fn unrelated_text_is_not_a_duplicate() {
        assert_eq!(
            classify("Deploy on Fridays", "Buy more coffee", Some(0.2)),
            None
        );
    }

    #[test]
    fn similarity_just_below_both_bars_is_not_a_duplicate() {
        assert_eq!(classify("alpha beta", "gamma delta", Some(0.899)), None);
    }

    #[test]
    fn query_terms_keep_order_drop_repeats_and_cap() {
        assert_eq!(
            query_terms("Bee a bee Cow cow"),
            vec!["bee".to_string(), "cow".to_string()]
        );
        let long: String = (0..100).map(|i| format!("w{i} ")).collect();
        assert_eq!(query_terms(&long).len(), MAX_QUERY_TERMS);
    }
}
