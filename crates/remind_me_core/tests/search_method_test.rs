//! Search says how it ran (#230, Mill Board #1186): each hit's
//! `search_method` and the response's `dormant_excluded`.

use chrono::{Duration, Utc};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::embedder::{EmbedError, EmbedRole, Embedder, EmbeddingIdentity};
use remind_me_core::testing;
use remind_me_core::vectors::embed_and_store;
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput, SearchMethod};
use std::collections::HashMap;

#[path = "../src/test_env.rs"]
mod test_env;

/// No configured embedder, HyDE or reranker: these tests pass their own.
fn offline() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        test_env::remove_var(remind_me_core::embedder::EMBEDDING_BACKEND_ENV);
        test_env::set_var(remind_me_core::query_expansion::EXPANSION_MODE_ENV, "off");
        test_env::set_var(remind_me_core::reranker::BACKEND_ENV, "");
    });
}

/// Every text lands on the same vector, so every embedded memory is a
/// semantic match whatever its words.
struct SameVector;

impl Embedder for SameVector {
    fn embed(&self, texts: &[String], _role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
    }
    fn dim(&self) -> usize {
        2
    }
    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "same-vector".into(),
            model: "test".into(),
            dim: 2,
        }
    }
}

fn add(store: &Store<'_>, content: &str, category: &str) -> String {
    offline();
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.to_string(),
            category: category.to_string(),
            extract: false,
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

fn embed(store: &Store<'_>, id: &str) {
    embed_and_store(store, &SameVector, id, "anything").unwrap();
}

fn age_by_days(store: &Store<'_>, id: &str, days: i64) {
    let when = (Utc::now() - Duration::days(days)).to_rfc3339();
    testing::set_memory_column(store, id, "accessed_at", when.as_str()).unwrap();
    testing::set_memory_column(store, id, "created_at", when).unwrap();
}

fn input(query: &str) -> MemorySearchInput {
    MemorySearchInput {
        query: query.to_string(),
        limit: 10,
        token_budget: 0,
        ..Default::default()
    }
}

fn methods(
    store: &Store<'_>,
    query: &str,
    embedder: Option<&dyn Embedder>,
) -> HashMap<String, Option<SearchMethod>> {
    offline();
    queries::search_memories_with_embedder(store, &input(query), embedder)
        .unwrap()
        .into_iter()
        .map(|r| (r.memory.id, r.search_method))
        .collect()
}

#[test]
fn without_an_embedder_every_hit_is_keyword() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "quokka sighting", "fact");
    let b = add(&store, "quokka burrow", "fact");

    let found = methods(&store, "quokka", None);
    assert_eq!(found[&a], Some(SearchMethod::Keyword));
    assert_eq!(found[&b], Some(SearchMethod::Keyword));
}

#[test]
fn with_an_embedder_hits_say_which_list_found_them() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let both = add(&store, "quokka sighting", "fact");
    let words_only = add(&store, "quokka burrow", "fact");
    let vector_only = add(&store, "wombat survey", "fact");
    embed(&store, &both);
    embed(&store, &vector_only);

    let found = methods(&store, "quokka", Some(&SameVector));
    assert_eq!(found[&both], Some(SearchMethod::Hybrid));
    assert_eq!(found[&words_only], Some(SearchMethod::Keyword));
    assert_eq!(found[&vector_only], Some(SearchMethod::Semantic));
}

#[test]
fn dormant_matches_are_counted_from_both_halves() {
    offline();
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka today", "fact");
    let by_words = add(&store, "quokka long ago", "action_item");
    let by_vector = add(&store, "wombat long ago", "action_item");
    embed(&store, &by_vector);
    age_by_days(&store, &by_words, 365);
    age_by_days(&store, &by_vector, 365);

    let hidden =
        queries::search_memories_budgeted(&store, &input("quokka"), Some(&SameVector)).unwrap();
    assert_eq!(hidden.dormant_excluded, 2);

    let shown = queries::search_memories_budgeted(
        &store,
        &MemorySearchInput {
            include_dormant: true,
            ..input("quokka")
        },
        Some(&SameVector),
    )
    .unwrap();
    assert_eq!(shown.dormant_excluded, 0);
}

#[test]
fn a_memory_below_the_callers_floor_is_not_counted_as_dormant() {
    offline();
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka today", "fact");
    let stale = add(&store, "quokka long ago", "action_item");
    age_by_days(&store, &stale, 365);

    let outcome = queries::search_memories_budgeted(
        &store,
        &MemorySearchInput {
            min_vitality: 0.99,
            ..input("quokka")
        },
        None,
    )
    .unwrap();
    assert_eq!(outcome.dormant_excluded, 0);
}
