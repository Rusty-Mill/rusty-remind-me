//! Coverage for the per-call retrieval strategy (gap T12, issue #106).
//!
//! The enum, the multiplier presets and the `Auto` router already existed —
//! `search_memories` simply hardcoded `RetrievalStrategy::Auto` instead of
//! reading the caller's choice. So most of what follows is characterisation:
//! pinning behaviour that was reachable only through an env var and a
//! hardcoded constant, now that it is part of the tool signature and callers
//! can depend on it.
//!
//! Weights are asserted as *ratios against the balanced profile* rather than
//! as absolute numbers. The presets are multipliers composed on top of
//! whatever `RrfWeights::from_env` resolved, deliberately so that a preset
//! cannot resurrect a signal an operator zeroed out — asserting absolutes
//! would bake in the default config and fail for anyone who tuned theirs.

use remind_me_core::retrieval::{
    choose_rrf_weights, looks_keyword_shaped, looks_semantic_shaped, looks_temporal_shaped,
};
use remind_me_core::RetrievalStrategy;

/// A query with no shape signal at all: three words, no punctuation, no
/// temporal keyword. `Auto` must fall through to balanced for these, which is
/// what makes it usable as a default.
const NEUTRAL: &str = "quokka beach photo";

fn weights(query: &str, strategy: RetrievalStrategy) -> (f64, f64, f64) {
    let w = choose_rrf_weights(query, strategy);
    (w.w_keyword, w.w_semantic, w.w_recency)
}

#[test]
fn balanced_leaves_every_signal_at_the_configured_baseline() {
    let (keyword, semantic, _) = weights(NEUTRAL, RetrievalStrategy::Balanced);
    let base = remind_me_core::retrieval::RrfWeights::from_env();

    // `Balanced` applies the identity multiplier, so it reproduces the live
    // configuration exactly rather than overriding it back to built-in
    // defaults. That distinction is the whole reason the presets are
    // multipliers: an operator who zeroed a signal keeps it zeroed.
    assert_eq!(keyword, base.w_keyword);
    assert_eq!(semantic, base.w_semantic);
}

#[test]
fn keyword_favored_raises_keyword_and_lowers_semantic() {
    let (bk, bs, _) = weights(NEUTRAL, RetrievalStrategy::Balanced);
    let (kk, ks, _) = weights(NEUTRAL, RetrievalStrategy::KeywordFavored);

    assert!(kk > bk, "keyword weight must rise: {} vs {}", kk, bk);
    assert!(ks < bs, "semantic weight must fall: {} vs {}", ks, bs);
}

#[test]
fn semantic_favored_is_the_mirror_image() {
    let (bk, bs, _) = weights(NEUTRAL, RetrievalStrategy::Balanced);
    let (sk, ss, _) = weights(NEUTRAL, RetrievalStrategy::SemanticFavored);

    assert!(sk < bk, "keyword weight must fall: {} vs {}", sk, bk);
    assert!(ss > bs, "semantic weight must rise: {} vs {}", ss, bs);
}

#[test]
fn a_pinned_preset_ignores_query_shape() {
    // The point of pinning one is to take the router out of the picture — for
    // A/B testing, or when the caller knows something the heuristic does not.
    // A preset that still bent to query shape would be useless for both.
    let quoted = weights("\"exact phrase\"", RetrievalStrategy::SemanticFavored);
    let neutral = weights(NEUTRAL, RetrievalStrategy::SemanticFavored);

    assert_eq!(
        quoted.0, neutral.0,
        "a quoted phrase must not pull a pinned semantic preset toward keyword"
    );
    assert_eq!(quoted.1, neutral.1);
}

// ---------------------------------------------------------------------------
// The Auto router's four branches
// ---------------------------------------------------------------------------

#[test]
fn auto_routes_a_quoted_phrase_to_keyword() {
    assert!(looks_keyword_shaped("\"exact phrase\""));
    let auto = weights("\"exact phrase\"", RetrievalStrategy::Auto);
    let pinned = weights("\"exact phrase\"", RetrievalStrategy::KeywordFavored);
    assert_eq!(auto, pinned);
}

#[test]
fn auto_routes_a_wildcard_to_keyword() {
    assert!(looks_keyword_shaped("quokk*"));
    let auto = weights("quokk*", RetrievalStrategy::Auto);
    let pinned = weights("quokk*", RetrievalStrategy::KeywordFavored);
    assert_eq!(auto, pinned);
}

#[test]
fn auto_routes_a_very_short_query_to_keyword() {
    // Two words or fewer. Someone typing "postgres migration" is naming a
    // thing, not asking a question, and exact terms are the better signal.
    assert!(looks_keyword_shaped("postgres migration"));
    let auto = weights("postgres migration", RetrievalStrategy::Auto);
    let pinned = weights("postgres migration", RetrievalStrategy::KeywordFavored);
    assert_eq!(auto, pinned);
}

#[test]
fn auto_routes_a_long_question_to_semantic() {
    let query = "why did we decide to move the scheduler out of the main process?";
    assert!(looks_semantic_shaped(query));
    let auto = weights(query, RetrievalStrategy::Auto);
    let pinned = weights(query, RetrievalStrategy::SemanticFavored);
    assert_eq!(auto, pinned);
}

#[test]
fn auto_falls_through_to_balanced_when_no_signal_fires() {
    assert!(!looks_keyword_shaped(NEUTRAL));
    assert!(!looks_semantic_shaped(NEUTRAL));

    // The fall-through is what makes Auto safe as the default: a query that
    // looks like nothing in particular gets the tuned baseline, not a guess.
    assert_eq!(
        weights(NEUTRAL, RetrievalStrategy::Auto),
        weights(NEUTRAL, RetrievalStrategy::Balanced)
    );
}

#[test]
fn keyword_shape_wins_when_both_heuristics_could_fire() {
    // A short *question* satisfies both: "?" is semantic-shaped, two words is
    // keyword-shaped. The router checks keyword first, so keyword wins. Pinned
    // here because the precedence is invisible in the code's shape — an
    // `if/else if` reordering would silently flip it.
    let query = "why postgres?";
    assert!(looks_keyword_shaped(query));
    assert!(looks_semantic_shaped(query));

    assert_eq!(
        weights(query, RetrievalStrategy::Auto),
        weights(query, RetrievalStrategy::KeywordFavored)
    );
}

// ---------------------------------------------------------------------------
// The temporal boost is orthogonal
// ---------------------------------------------------------------------------

#[test]
fn a_temporal_query_boosts_recency_under_every_strategy() {
    // Applied after the profile, not as a fifth profile — "what did we decide
    // last week" is both a semantic question and a recency-sensitive one, and
    // the two signals should compose rather than one replacing the other.
    let temporal = "what did we decide about the schema last week";
    assert!(looks_temporal_shaped(temporal));

    for strategy in [
        RetrievalStrategy::Auto,
        RetrievalStrategy::Balanced,
        RetrievalStrategy::KeywordFavored,
        RetrievalStrategy::SemanticFavored,
    ] {
        let boosted = choose_rrf_weights(temporal, strategy).w_recency;
        let plain = choose_rrf_weights(NEUTRAL, strategy).w_recency;
        assert!(
            boosted > plain,
            "{:?}: recency should be boosted ({} vs {})",
            strategy,
            boosted,
            plain
        );
    }
}

// ---------------------------------------------------------------------------
// The wire contract
// ---------------------------------------------------------------------------

#[test]
fn the_strategy_field_defaults_to_auto_and_parses_the_reference_spellings() {
    use remind_me_core::MemorySearchInput;

    let absent: MemorySearchInput =
        serde_json::from_value(serde_json::json!({ "query": "x" })).unwrap();
    assert_eq!(
        absent.strategy,
        RetrievalStrategy::Auto,
        "the reference defaults to auto; an old caller must land there too"
    );

    for (wire, want) in [
        ("auto", RetrievalStrategy::Auto),
        ("balanced", RetrievalStrategy::Balanced),
        ("keyword_favored", RetrievalStrategy::KeywordFavored),
        ("semantic_favored", RetrievalStrategy::SemanticFavored),
    ] {
        let input: MemorySearchInput =
            serde_json::from_value(serde_json::json!({ "query": "x", "strategy": wire })).unwrap();
        assert_eq!(input.strategy, want, "for wire value {:?}", wire);
    }
}
