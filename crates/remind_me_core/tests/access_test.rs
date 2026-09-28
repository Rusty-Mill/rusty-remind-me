//! Coverage for access recording — the half of the vitality model that was
//! inert until now.

use chrono::{Duration, Utc};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::testing;
use remind_me_core::vitality::{record_accesses, BRIDGE_THRESHOLD};
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput};

fn add(store: &Store<'_>, content: &str, category: &str) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: category.to_string(),
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

fn search(store: &Store<'_>, query: &str) -> Vec<String> {
    queries::search_with_expansions(store, &input(query))
        .unwrap()
        .memories
        .iter()
        .map(|r| r.memory.id.clone())
        .collect()
}

fn access_count(store: &Store<'_>, id: &str) -> i64 {
    testing::memory_i64(store, id, "access_count")
        .unwrap()
        .unwrap()
}

fn accessed_at(store: &Store<'_>, id: &str) -> String {
    testing::memory_text(store, id, "accessed_at")
        .unwrap()
        .unwrap()
}

fn backdate(store: &Store<'_>, id: &str, days: i64) {
    let when = (Utc::now() - Duration::days(days)).to_rfc3339();
    testing::set_memory_column(store, id, "accessed_at", when.clone()).unwrap();
    testing::set_memory_column(store, id, "created_at", when).unwrap();
}

#[test]
fn retrieval_increments_the_count_and_moves_the_stamp() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "quokka sighting", "fact");
    backdate(&store, &id, 30);
    let before = accessed_at(&store, &id);
    assert_eq!(access_count(&store, &id), 0);

    search(&store, "quokka");

    assert_eq!(access_count(&store, &id), 1);
    assert!(accessed_at(&store, &id) > before);
}

#[test]
fn repeated_retrieval_keeps_counting() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "quokka sighting", "fact");

    for _ in 0..5 {
        search(&store, "quokka");
    }

    assert_eq!(access_count(&store, &id), 5);
}

#[test]
fn a_memory_in_regular_use_outlives_an_abandoned_one() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let used = add(&store, "quokka in use", "action_item");
    let abandoned = add(&store, "quokka abandoned", "action_item");
    // Both written a month ago; at 0.20 decay that is dormant.
    backdate(&store, &used, 30);
    backdate(&store, &abandoned, 30);

    // One of them is retrieved today.
    queries::search_with_expansions(
        &store,
        &MemorySearchInput {
            strategy: Default::default(),
            include_sensitive: false,
            query: "use".into(),
            ..input("use")
        },
    )
    .unwrap();

    let mut live = input("quokka");
    live.include_dormant = false;
    let found: Vec<String> = queries::search_with_expansions(&store, &live)
        .unwrap()
        .memories
        .iter()
        .map(|r| r.memory.id.clone())
        .collect();

    // This is the point of the whole feature: dormancy has to measure time
    // since last *use*, not since writing. Before access recording both of
    // these decayed identically.
    assert_eq!(found, vec![used]);
    assert!(!found.contains(&abandoned));
}

#[test]
fn bridge_protection_becomes_reachable() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "quokka sighting", "fact");

    for _ in 0..BRIDGE_THRESHOLD {
        search(&store, "quokka");
    }

    // Nothing could reach the bridge threshold before — the only test for it
    // set the column by hand.
    assert_eq!(access_count(&store, &id), BRIDGE_THRESHOLD);
}

#[test]
fn the_stored_vitality_reflects_the_new_count() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "quokka sighting", "general");
    let before: f64 = testing::memory_f64(&store, &id, "vitality")
        .unwrap()
        .unwrap();

    search(&store, "quokka");

    let after: f64 = testing::memory_f64(&store, &id, "vitality")
        .unwrap()
        .unwrap();
    // Recomputed at zero elapsed days, so it collapses to
    // base_weight * sqrt(count + 1): 1.0 * sqrt(2).
    assert!((after - 2.0_f64.sqrt()).abs() < 1e-9, "got {}", after);
    assert!(after > before);
}

#[test]
fn status_is_maintained() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "quokka sighting", "general");

    search(&store, "quokka");

    let status: String = testing::memory_text(&store, &id, "status")
        .unwrap()
        .unwrap();
    // Nothing wrote this column before; the earlier tests pinned it at
    // "active" by observation rather than because anything maintained it.
    assert_eq!(status, "active");
}

#[test]
fn expansion_results_are_not_recorded() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let seed = add(&store, "quokka sighting", "fact");
    let neighbour = add(&store, "wholly different wording", "fact");
    // Associate them without either being a search hit for the query below.
    remind_me_core::expansion::record_co_retrieval(&store, &[seed.clone(), neighbour.clone()])
        .unwrap();

    let mut expanded = input("quokka");
    expanded.expand_co_retrieval = true;
    let result = queries::search_with_expansions(&store, &expanded).unwrap();
    assert_eq!(result.related_via_co_retrieval.unwrap().len(), 1);

    assert_eq!(access_count(&store, &seed), 1, "a direct hit is recorded");
    assert_eq!(
        access_count(&store, &neighbour),
        0,
        "an expansion is a discovery aid, not an answer to the query; \
         recording it would inflate every neighbour on every expanded search"
    );
}

#[test]
fn a_plain_search_records_nothing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "quokka sighting", "fact");

    queries::search_memories(&store, &input("quokka")).unwrap();

    assert_eq!(
        access_count(&store, &id),
        0,
        "search_memories is a pure read; the write lives in the wrapper"
    );
}

#[test]
fn unknown_ids_are_skipped() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let real = add(&store, "quokka sighting", "fact");

    let updated = record_accesses(&store, &["mem_ghost".to_string(), real.clone()]).unwrap();

    assert_eq!(updated, 1);
    assert_eq!(access_count(&store, &real), 1);
}

#[test]
fn recording_nothing_is_a_no_op() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(record_accesses(&db.store(), &[]).unwrap(), 0);
}

#[test]
fn a_search_that_matches_nothing_records_nothing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "quokka sighting", "fact");

    assert!(search(&store, "wombat").is_empty());

    assert_eq!(access_count(&store, &id), 0);
}
