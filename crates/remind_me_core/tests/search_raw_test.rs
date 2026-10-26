//! Raw session dialogs leave the ranking by default (ADR-0029).
//!
//! A captured dialog runs to 200,000 characters and matches almost any query,
//! so it would crowd out the facts drawn from it. These check that it stays
//! reachable: on request, by category, and as a marked fallback when nothing
//! refined matches.

use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::models::MemorySearchInput;
use remind_me_core::{Database, MemoryAddInput};

fn add(store: &Store<'_>, content: &str, category: &str) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.into(),
            category: category.into(),
            source: "manual".into(),
            metadata: serde_json::json!({}),
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

fn ids(store: &Store<'_>, input: MemorySearchInput) -> (Vec<String>, bool) {
    let res = queries::search_with_expansions(store, &input).unwrap();
    let ids = res.memories.iter().map(|r| r.memory.id.clone()).collect();
    (ids, res.raw_fallback)
}

fn query(text: &str) -> MemorySearchInput {
    MemorySearchInput {
        query: text.into(),
        ..Default::default()
    }
}

#[test]
fn a_dialog_ranks_only_when_asked_for() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let fact = add(
        &store,
        "quokka releases merge with merge commits",
        "decision",
    );
    let dialog = add(
        &store,
        "user: how do quokka releases merge? assistant: merge commits",
        "dialog",
    );

    let (found, fallback) = ids(&store, query("quokka"));
    assert_eq!(found, vec![fact.clone()]);
    assert!(!fallback);

    let (found, _) = ids(
        &store,
        MemorySearchInput {
            include_raw: true,
            ..query("quokka")
        },
    );
    assert!(found.contains(&fact) && found.contains(&dialog));

    let (found, fallback) = ids(
        &store,
        MemorySearchInput {
            category: Some("dialog".into()),
            ..query("quokka")
        },
    );
    assert_eq!((found, fallback), (vec![dialog], false));
}

#[test]
fn nothing_refined_falls_back_to_marked_dialogs() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(
        &store,
        "quokka releases merge with merge commits",
        "decision",
    );
    let dialog = add(&store, "user: the wombat build is red", "dialog");

    let (found, fallback) = ids(&store, query("wombat"));
    assert_eq!((found, fallback), (vec![dialog], true));

    let (found, fallback) = ids(&store, query("platypus"));
    assert!(found.is_empty());
    assert!(!fallback, "an empty fallback is not reported as one");
}

#[test]
fn a_session_or_raw_category_turns_raw_ranking_on() {
    let mut input = query("x");
    assert!(!input.ranks_raw());
    input.scope.session_id = Some("s1".into());
    assert!(input.ranks_raw());
    let skeleton = MemorySearchInput {
        category: Some("skeleton".into()),
        ..query("x")
    };
    assert!(skeleton.ranks_raw());
}

/// Points every query, raw text and `strong` distractor one way and
/// everything else close by, so those are the stronger semantic hits without
/// any keyword overlap.
struct RawLeaningEmbedder;

impl remind_me_core::embedder::Embedder for RawLeaningEmbedder {
    fn embed(
        &self,
        texts: &[String],
        role: remind_me_core::embedder::EmbedRole,
    ) -> Result<Vec<Vec<f32>>, remind_me_core::embedder::EmbedError> {
        use remind_me_core::embedder::EmbedRole;
        Ok(texts
            .iter()
            .map(|t| match role {
                EmbedRole::Query => vec![1.0, 0.0],
                EmbedRole::Passage if t.contains("dialog") || t.contains("strong") => {
                    vec![1.0, 0.0]
                }
                EmbedRole::Passage => vec![0.8, 0.6],
            })
            .collect())
    }
    fn dim(&self) -> usize {
        2
    }
    fn identity(&self) -> remind_me_core::embedder::EmbeddingIdentity {
        remind_me_core::embedder::EmbeddingIdentity {
            backend: "raw-leaning".into(),
            model: "test".into(),
            dim: 2,
        }
    }
}

fn embedded(store: &Store<'_>, content: &str, category: &str) -> String {
    let id = add(store, content, category);
    remind_me_core::vectors::embed_and_store(store, &RawLeaningEmbedder, &id, content).unwrap();
    id
}

#[test]
fn stronger_raw_hits_do_not_crowd_a_refined_semantic_match_out() {
    use remind_me_core::retrieval::Deadline;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for n in 0..5 {
        embedded(&store, &format!("dialog number {n}"), "dialog");
    }
    let fact = embedded(&store, "striped horses graze", "fact");
    let input = MemorySearchInput {
        limit: 1,
        ..query("zebra")
    };

    let outcome = queries::search_memories_deadlined(
        &store,
        &input,
        Some(&RawLeaningEmbedder),
        Deadline::unbounded(),
    )
    .unwrap();

    let found: Vec<&str> = outcome
        .results
        .iter()
        .map(|r| r.memory.id.as_str())
        .collect();
    assert_eq!(found, vec![fact.as_str()]);
    assert!(!outcome.raw_fallback);
}

#[test]
fn a_fallback_past_the_deadline_still_reports_the_skipped_semantic_stage() {
    use remind_me_core::retrieval::Deadline;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let dialog = embedded(&store, "dialog about the zebra crossing", "dialog");

    let outcome = queries::search_memories_deadlined(
        &store,
        &query("zebra"),
        Some(&RawLeaningEmbedder),
        Deadline::already_passed(),
    )
    .unwrap();

    let found: Vec<&str> = outcome
        .results
        .iter()
        .map(|r| r.memory.id.as_str())
        .collect();
    assert_eq!(found, vec![dialog.as_str()]);
    assert!(outcome.raw_fallback);
    assert!(
        outcome.timing.skipped.iter().any(|s| s == "semantic"),
        "{:?}",
        outcome.timing
    );
    assert!(outcome.timing.degraded());
}

fn last_day() -> remind_me_core::time_window::TimeWindow {
    use remind_me_core::time_window::{RawTimeWindow, TimeField, TimeWindow};
    let raw = RawTimeWindow {
        since: Some("1d".into()),
        until: None,
        time_field: TimeField::Created,
    };
    TimeWindow::resolve(&raw, chrono::Utc::now()).unwrap()
}

fn aged(store: &Store<'_>, id: &str, days: i64) {
    let when = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    remind_me_core::testing::set_memory_column(store, id, "created_at", when).unwrap();
}

#[test]
fn a_window_listing_leaves_dialogs_out_while_refined_rows_are_in_it() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let fact = add(&store, "wombat burrows face north", "fact");
    add(&store, "user: which way do wombat burrows face?", "dialog");
    let input = MemorySearchInput {
        window: last_day(),
        include_dormant: true,
        ..query("")
    };

    let (found, fallback) = ids(&store, input);

    assert_eq!((found, fallback), (vec![fact], false));
}

#[test]
fn a_window_with_only_dialogs_lists_them_as_raw_fallback() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old_fact = add(&store, "wombat burrows face north", "fact");
    aged(&store, &old_fact, 3);
    let dialog = add(&store, "user: which way do wombat burrows face?", "dialog");
    let input = MemorySearchInput {
        window: last_day(),
        include_dormant: true,
        ..query("")
    };

    let (found, fallback) = ids(&store, input);

    assert_eq!((found, fallback), (vec![dialog], true));
}

#[test]
fn a_windowed_keyword_search_falls_back_only_to_dialogs_in_the_window() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old_fact = add(&store, "wombat burrows face north", "fact");
    aged(&store, &old_fact, 3);
    let old_dialog = add(&store, "user: wombat burrows again", "dialog");
    aged(&store, &old_dialog, 3);
    let dialog = add(&store, "user: which way do wombat burrows face?", "dialog");
    let input = MemorySearchInput {
        window: last_day(),
        include_dormant: true,
        ..query("wombat")
    };

    let (found, fallback) = ids(&store, input);

    assert_eq!((found, fallback), (vec![dialog], true));
}

#[test]
fn semantic_raw_exclusion_composes_with_the_window() {
    use remind_me_core::retrieval::Deadline;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for n in 0..5 {
        embedded(&store, &format!("dialog number {n}"), "dialog");
    }
    let old_fact = embedded(&store, "striped horses graze far away", "fact");
    aged(&store, &old_fact, 3);
    let fact = embedded(&store, "striped horses graze", "fact");
    let input = MemorySearchInput {
        limit: 1,
        window: last_day(),
        include_dormant: true,
        ..query("zebra")
    };

    let outcome = queries::search_memories_deadlined(
        &store,
        &input,
        Some(&RawLeaningEmbedder),
        Deadline::unbounded(),
    )
    .unwrap();

    let found: Vec<&str> = outcome
        .results
        .iter()
        .map(|r| r.memory.id.as_str())
        .collect();
    assert_eq!(found, vec![fact.as_str()]);
    assert!(!outcome.raw_fallback);
}

/// A row in project `mill`, tagged `zoo`, made `days` ago: what the filtered
/// searches below admit, unless a field is overridden.
fn row(id: &str, content: &str, category: &str, days: i64) -> NewMemory {
    let when = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    NewMemory {
        category: category.into(),
        tags: vec!["zoo".into()],
        project: Some("mill".into()),
        ..NewMemory::new(id, content, &when)
    }
}

fn put(store: &Store<'_>, memory: NewMemory) {
    Memories::new(store).insert(&memory).unwrap();
    remind_me_core::vectors::embed_and_store(
        store,
        &RawLeaningEmbedder,
        &memory.id,
        &memory.content,
    )
    .unwrap();
}

/// Window, scope and tag filter at once, the `zoo` tag and `mill` project
/// set by [`row`].
fn filtered(text: &str) -> MemorySearchInput {
    MemorySearchInput {
        tags: Some(vec!["zoo".into()]),
        scope: remind_me_core::context::ScopeFilter {
            project: Some("mill".into()),
            ..Default::default()
        },
        window: last_day(),
        include_dormant: true,
        ..query(text)
    }
}

#[test]
fn every_filter_and_raw_exclusion_admit_before_the_semantic_limit() {
    use remind_me_core::retrieval::Deadline;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // Three of each kind, more than the `limit * 2` the semantic scan keeps,
    // all stronger semantic hits than the one eligible fact.
    for n in 0..3 {
        put(
            &store,
            row(&format!("raw{n}"), "dialog strong", "dialog", 0),
        );
        put(&store, row(&format!("old{n}"), "strong old", "fact", 3));
        put(
            &store,
            NewMemory {
                project: Some("elsewhere".into()),
                ..row(&format!("out{n}"), "strong elsewhere", "fact", 0)
            },
        );
        put(
            &store,
            NewMemory {
                tags: Vec::new(),
                ..row(&format!("bare{n}"), "strong untagged", "fact", 0)
            },
        );
    }
    put(&store, row("fact", "striped horses graze", "fact", 0));
    let input = MemorySearchInput {
        limit: 1,
        ..filtered("zebra")
    };

    let outcome = queries::search_memories_deadlined(
        &store,
        &input,
        Some(&RawLeaningEmbedder),
        Deadline::unbounded(),
    )
    .unwrap();

    let found: Vec<&str> = outcome
        .results
        .iter()
        .map(|r| r.memory.id.as_str())
        .collect();
    assert_eq!(found, vec!["fact"]);
    assert!(!outcome.raw_fallback);
}

#[test]
fn a_filtered_queryless_fallback_keeps_every_filter_detail_and_deadline() {
    use remind_me_core::models::SearchDetail;
    use remind_me_core::retrieval::Deadline;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let long = "dialog ".repeat(300);
    put(&store, row("old-fact", "striped horses graze", "fact", 3));
    put(&store, row("raw-a", &long, "dialog", 0));
    put(&store, row("raw-b", &long, "dialog", 0));
    put(&store, row("raw-old", &long, "dialog", 3));
    put(
        &store,
        NewMemory {
            project: Some("elsewhere".into()),
            ..row("raw-out", &long, "dialog", 0)
        },
    );
    put(
        &store,
        NewMemory {
            tags: Vec::new(),
            ..row("raw-bare", &long, "dialog", 0)
        },
    );
    // Two compact hits fit 200 tokens; two full ones would not.
    let input = MemorySearchInput {
        detail: SearchDetail::Compact,
        token_budget: 200,
        ..filtered("")
    };

    let outcome = queries::search_memories_deadlined(
        &store,
        &input,
        Some(&RawLeaningEmbedder),
        Deadline::already_passed(),
    )
    .unwrap();

    let mut found: Vec<&str> = outcome
        .results
        .iter()
        .map(|r| r.memory.id.as_str())
        .collect();
    found.sort();
    assert_eq!(found, vec!["raw-a", "raw-b"]);
    assert!(outcome.raw_fallback);
    assert_eq!(outcome.trimmed, 0);
}
