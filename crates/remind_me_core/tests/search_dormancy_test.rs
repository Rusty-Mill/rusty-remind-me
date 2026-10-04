//! Coverage for dormancy filtering in `remind_me_search` (the `DI-04` fix).
//!
//! Age is simulated by backdating `accessed_at`, because nothing in the crate
//! updates it after insert and there is no clock to move.

use chrono::{Duration, Utc};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::testing;
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput};

fn add(store: &Store<'_>, content: &str, category: &str) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            extract: true,
            attachments: vec![],
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

fn age_by_days(store: &Store<'_>, id: &str, days: i64) {
    let when = (Utc::now() - Duration::days(days)).to_rfc3339();
    testing::set_memory_column(store, id, "accessed_at", when.as_str()).unwrap();
    testing::set_memory_column(store, id, "created_at", when).unwrap();
}

fn search(store: &Store<'_>, query: &str, include_dormant: bool, min_vitality: f64) -> Vec<String> {
    let input = MemorySearchInput {
        strategy: Default::default(),
        include_sensitive: false,
        query: query.to_string(),
        category: None,
        tags: None,
        limit: 50,
        token_budget: 100_000,
        response_format: Default::default(),
        include_dormant,
        min_vitality,
        verbose: false,
        expand_entities: false,
        include_neighbors: false,
        expand_co_retrieval: false,
        bootstrap: false,
    };
    queries::search_memories(store, &input)
        .unwrap()
        .into_iter()
        .map(|r| r.memory.id)
        .collect()
}

#[test]
fn an_aged_memory_is_filtered_out_by_default() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let fresh = add(&store, "quokka sighting today", "fact");
    let stale = add(&store, "quokka sighting long ago", "action_item");
    age_by_days(&store, &stale, 365);

    // Before this fix the filter compared the stored `vitality` column, which
    // never decays — so `include_dormant: false` filtered nothing at all.
    let found = search(&store, "quokka", false, 0.0);
    assert_eq!(found, vec![fresh], "the year-old memory should be dormant");
    assert!(!found.contains(&stale));
}

#[test]
fn include_dormant_brings_it_back() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let fresh = add(&store, "quokka today", "fact");
    let stale = add(&store, "quokka long ago", "action_item");
    age_by_days(&store, &stale, 365);

    let mut found = search(&store, "quokka", true, 0.0);
    found.sort();
    let mut expected = vec![fresh, stale];
    expected.sort();
    assert_eq!(found, expected);
}

#[test]
fn a_fresh_memory_is_never_filtered() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "quokka today", "action_item");

    assert_eq!(search(&store, "quokka", false, 0.0), vec![id]);
}

#[test]
fn slow_decaying_categories_outlive_fast_ones() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // decision decays at 0.02, action_item at 0.20.
    let durable = add(&store, "quokka decision", "decision");
    let fragile = add(&store, "quokka action", "action_item");
    age_by_days(&store, &durable, 60);
    age_by_days(&store, &fragile, 60);

    let found = search(&store, "quokka", false, 0.0);
    assert!(
        found.contains(&durable),
        "a decision should survive two months"
    );
    assert!(
        !found.contains(&fragile),
        "an action item should not; decay rate must actually matter"
    );
}

#[test]
fn min_vitality_compares_against_the_decayed_value() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "quokka notes", "fact");
    age_by_days(&store, &id, 30);

    // A fresh "fact" starts at base_weight 1.15; thirty days at 0.05 decay puts
    // it near 0.26. A threshold between those two only discriminates if the
    // comparison is against the decayed value rather than the stored snapshot.
    assert!(
        search(&store, "quokka", true, 0.9).is_empty(),
        "0.9 is above the decayed value, so nothing should match"
    );
    assert_eq!(
        search(&store, "quokka", true, 0.1),
        vec![id],
        "0.1 is below it, so the memory should still be found"
    );
}

#[test]
fn bridge_protection_keeps_a_heavily_used_memory_searchable() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let plain = add(&store, "quokka rarely used", "action_item");
    let bridge = add(&store, "quokka heavily used", "action_item");
    // Thirty days is the interval where the halving is what decides it. An
    // action_item starts at base_weight 1.0 and decays at 0.20, so untouched it
    // reaches exp(-6) ≈ 0.002 — dormant. Ten accesses both boost it by sqrt(11)
    // and halve the decay: 3.32 * exp(-3) ≈ 0.17, comfortably above the floor.
    // The boost alone would not save it (3.32 * exp(-6) ≈ 0.008), so this fails
    // if the CASE on access_count is dropped.
    age_by_days(&store, &plain, 30);
    age_by_days(&store, &bridge, 30);
    // At or above BRIDGE_THRESHOLD accesses, decay is halved.
    testing::set_memory_column(&store, &bridge, "access_count", 10).unwrap();

    let found = search(&store, "quokka", false, 0.0);
    assert!(
        found.contains(&bridge),
        "bridge protection applies in SQL too"
    );
    assert!(!found.contains(&plain));
}

#[test]
fn a_null_accessed_at_falls_back_to_created_at() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "quokka synced", "fact");
    // A row written by `remind_me` can leave the column unset — its schema
    // default is NULL. Without a fallback the vitality expression would be NULL
    // and the row would silently vanish from every search.
    testing::set_memory_column(&store, &id, "accessed_at", testing::Value::Null).unwrap();

    assert_eq!(search(&store, "quokka", false, 0.0), vec![id.clone()]);
    // Parsing the row must not fail either.
    assert!(queries::get_memory_by_id(&store, &id).unwrap().is_some());
}

#[test]
fn the_filter_runs_before_the_limit_so_pages_stay_full() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // Far more dormant rows than live ones. If the filter ran after LIMIT the
    // live memories would be crowded out and the page would come back short.
    for i in 0..40 {
        let id = add(&store, &format!("quokka dormant {}", i), "action_item");
        age_by_days(&store, &id, 365);
    }
    let mut live = Vec::new();
    for i in 0..5 {
        live.push(add(&store, &format!("quokka live {}", i), "fact"));
    }

    let mut found = search(&store, "quokka", false, 0.0);
    found.sort();
    live.sort();
    assert_eq!(found, live, "every live memory should still be returned");
}
