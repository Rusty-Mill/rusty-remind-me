//! A caller can supply its own embedder, rather than getting whichever one
//! `available_embedder` happens to resolve.
//!
//! `Embedder`'s own doc says it is a trait "so a future backend
//! (ONNX-in-process, say) can be added without touching anything that already
//! depends on this". That was not quite true while `search_memories` reached
//! for the concrete resolver itself — a caller holding a working
//! `impl Embedder` had no way to get it in. These tests cover the seam.

use remind_me_core::db::queries;
use remind_me_core::embedder::{EmbedError, EmbedRole, Embedder, EmbeddingIdentity};
use remind_me_core::vectors::embed_and_store;
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput};
use rusqlite::Connection;

/// An embedder with no daemon, no network, and no model file.
///
/// Deterministic by construction: the vector is a bag-of-characters histogram,
/// so the same text always embeds to the same point. That is the property the
/// injectable seam exists to make available — a caller that needs the same
/// query to return the same rows cannot get it from a probed backend.
struct CharHistogramEmbedder;

const DIM: usize = 26;

impl Embedder for CharHistogramEmbedder {
    fn embed(&self, texts: &[String], _role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut counts = vec![0f32; DIM];
                for ch in text.to_ascii_lowercase().chars() {
                    if ch.is_ascii_lowercase() {
                        counts[(ch as u8 - b'a') as usize] += 1.0;
                    }
                }
                let norm = counts.iter().map(|c| c * c).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for count in &mut counts {
                        *count /= norm;
                    }
                }
                counts
            })
            .collect())
    }

    fn dim(&self) -> usize {
        DIM
    }

    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "char-histogram".into(),
            model: "test".into(),
            dim: DIM,
        }
    }
}

fn add(conn: &Connection, content: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap()
    .id
}

fn input(query: &str) -> MemorySearchInput {
    MemorySearchInput {
        strategy: Default::default(),
        include_sensitive: false,
        query: query.to_string(),
        category: None,
        tags: None,
        limit: 20,
        token_budget: 100_000,
        response_format: Default::default(),
        include_dormant: true,
        min_vitality: 0.0,
        verbose: false,
        expand_entities: false,
        include_neighbors: false,
        expand_co_retrieval: false,
        bootstrap: false,
    }
}

/// A store with vectors written by the supplied embedder, so the semantic half
/// has something to match against.
fn seeded() -> Database {
    let db = Database::open_in_memory().unwrap();
    {
        let conn = db.conn();
        for content in [
            "the deploy key rotates every ninety days",
            "staging mirrors production except the cache tier",
            "releases are cut from main on fridays",
        ] {
            let id = add(&conn, content);
            embed_and_store(&conn, &CharHistogramEmbedder, &id, content).unwrap();
        }
    }
    db
}

#[test]
fn a_supplied_embedder_is_used() {
    // The query shares no *term* with any memory, so the keyword half finds
    // nothing and cannot account for the result. Asserting on a query that
    // FTS would also match would pass whether or not the embedder was ever
    // consulted — which is the whole thing this test exists to rule out.
    let db = seeded();
    let conn = db.conn();
    let query = "ydolep yek rotaets";

    let without = queries::search_memories_with_embedder(&conn, &input(query), None).unwrap();
    assert!(
        without.is_empty(),
        "the keyword half matched, so this query cannot isolate the embedder: {:?}",
        without
            .iter()
            .map(|r| &r.memory.content)
            .collect::<Vec<_>>()
    );

    let with =
        queries::search_memories_with_embedder(&conn, &input(query), Some(&CharHistogramEmbedder))
            .unwrap();
    assert!(
        !with.is_empty(),
        "the supplied embedder was not consulted: keyword-only and embedder-backed \
         searches returned the same nothing"
    );
}

#[test]
fn none_is_keyword_only_rather_than_an_error() {
    // The explicit form of what a failed probe produces today. A caller that
    // wants reproducibility asks for it here instead of hoping the daemon is
    // down consistently.
    let db = seeded();
    let conn = db.conn();

    let results =
        queries::search_memories_with_embedder(&conn, &input("deploy key"), None).unwrap();

    assert!(
        results
            .iter()
            .any(|r| r.memory.content.contains("deploy key")),
        "keyword search should still answer without an embedder"
    );
}

#[test]
fn the_same_query_returns_the_same_order_every_time() {
    // The property the seam exists for. With a probed backend this can flip
    // between calls depending on whether the daemon answered.
    let db = seeded();
    let conn = db.conn();

    let ids = |embedder: Option<&dyn Embedder>| -> Vec<String> {
        queries::search_memories_with_embedder(&conn, &input("cache tier on fridays"), embedder)
            .unwrap()
            .iter()
            .map(|r| r.memory.id.clone())
            .collect()
    };

    let first = ids(Some(&CharHistogramEmbedder));
    assert_eq!(first, ids(Some(&CharHistogramEmbedder)));
    assert!(!first.is_empty());
}

#[test]
fn search_memories_still_resolves_the_configured_backend() {
    // The wrapper's contract: unchanged behaviour for every existing caller.
    // No backend is configured in this test process, so `available_embedder`
    // yields `None` and this is the keyword-only path — the same answer the
    // explicit `None` above gets.
    let db = seeded();
    let conn = db.conn();

    let wrapped = queries::search_memories(&conn, &input("deploy key")).unwrap();
    let explicit =
        queries::search_memories_with_embedder(&conn, &input("deploy key"), None).unwrap();

    let ids = |rows: &[remind_me_core::MemorySearchResult]| -> Vec<String> {
        rows.iter().map(|r| r.memory.id.clone()).collect()
    };
    assert_eq!(ids(&wrapped), ids(&explicit));
}
