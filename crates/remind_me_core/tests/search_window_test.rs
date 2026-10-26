//! Coverage for `remind_me_search`'s `since`/`until` window.
//!
//! Age is simulated by backdating `created_at`/`updated_at`; there is no
//! clock to move.

use chrono::{Duration, Utc};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::testing;
use remind_me_core::time_window::{RawTimeWindow, TimeField, TimeWindow};
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput};

fn add(store: &Store<'_>, content: &str) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.to_string(),
            category: "fact".to_string(),
            source: "manual".into(),
            metadata: serde_json::json!({}),
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

fn age(store: &Store<'_>, id: &str, column: &str, hours: i64) {
    let when = (Utc::now() - Duration::hours(hours)).to_rfc3339();
    testing::set_memory_column(store, id, column, when).unwrap();
}

fn window(since: Option<&str>, until: Option<&str>, field: TimeField) -> TimeWindow {
    let raw = RawTimeWindow {
        since: since.map(str::to_string),
        until: until.map(str::to_string),
        time_field: field,
    };
    TimeWindow::resolve(&raw, Utc::now()).unwrap()
}

fn search(store: &Store<'_>, query: &str, window: TimeWindow) -> Vec<String> {
    let input = MemorySearchInput {
        query: query.to_string(),
        limit: 50,
        token_budget: 100_000,
        include_dormant: true,
        window,
        ..Default::default()
    };
    queries::search_memories(store, &input)
        .unwrap()
        .into_iter()
        .map(|r| r.memory.id)
        .collect()
}

/// Three wombat notes created 1h, 30h and 80h ago.
fn seeded(store: &Store<'_>) -> [String; 3] {
    let ids = ["wombat one", "wombat two", "wombat three"].map(|c| add(store, c));
    for (id, hours) in ids.iter().zip([1, 30, 80]) {
        age(store, id, "created_at", hours);
    }
    ids
}

#[test]
fn keyword_search_is_narrowed_to_the_window() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let [recent, middle, old] = seeded(&store);

    let hits = search(
        &store,
        "wombat",
        window(Some("2d"), None, TimeField::Created),
    );
    assert!(hits.contains(&recent) && hits.contains(&middle), "{hits:?}");
    assert!(!hits.contains(&old), "{hits:?}");

    let hits = search(
        &store,
        "wombat",
        window(Some("2d"), Some("1d"), TimeField::Created),
    );
    assert_eq!(hits, vec![middle]);
}

#[test]
fn empty_query_lists_the_window_newest_first() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let [recent, middle, old] = seeded(&store);

    let hits = search(&store, "", window(Some("1w"), None, TimeField::Created));
    assert_eq!(hits, vec![recent, middle, old]);
}

#[test]
fn empty_query_without_a_window_still_matches_nothing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    seeded(&store);
    assert!(search(&store, "", TimeWindow::default()).is_empty());
    assert!(search(&store, "   ", TimeWindow::default()).is_empty());
}

#[test]
fn listing_respects_limit_and_the_other_filters() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let [recent, _, _] = seeded(&store);
    let input = MemorySearchInput {
        query: String::new(),
        limit: 1,
        include_dormant: true,
        window: window(Some("1w"), None, TimeField::Created),
        ..Default::default()
    };
    let hits = queries::search_memories(&store, &input).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory.id, recent);

    let other_category = MemorySearchInput {
        category: Some("preference".into()),
        limit: 50,
        ..input
    };
    assert!(queries::search_memories(&store, &other_category)
        .unwrap()
        .is_empty());
}

#[test]
fn time_field_updated_reads_updated_at() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let [recent, middle, old] = seeded(&store);
    // The old note was edited an hour ago; the other two long ago.
    age(&store, &old, "updated_at", 1);
    age(&store, &recent, "updated_at", 100);
    age(&store, &middle, "updated_at", 100);

    let hits = search(
        &store,
        "wombat",
        window(Some("2h"), None, TimeField::Updated),
    );
    assert_eq!(hits, vec![old]);
}

#[test]
fn the_window_is_independent_of_include_expired() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let [recent, middle, _] = seeded(&store);
    // The newest note is inside the window, but its validity has lapsed.
    testing::set_memory_column(&store, &recent, "valid_until", "2020-01-01T00:00:00Z").unwrap();
    let in_window = window(Some("2d"), None, TimeField::Created);

    let kept = search(&store, "", in_window);
    assert_eq!(
        kept,
        vec![middle.clone(), recent],
        "expired ranks last, still listed"
    );

    let input = MemorySearchInput {
        query: String::new(),
        limit: 50,
        include_dormant: true,
        include_expired: false,
        window: in_window,
        ..Default::default()
    };
    let ids: Vec<String> = queries::search_memories(&store, &input)
        .unwrap()
        .into_iter()
        .map(|r| r.memory.id)
        .collect();
    assert_eq!(ids, vec![middle]);
}

/// One-slot listing over `[recent, middle, _]` with `recent` made ineligible.
fn first_listed(store: &Store<'_>, include_expired: bool, min_confidence: f64) -> Vec<String> {
    let input = MemorySearchInput {
        query: String::new(),
        limit: 1,
        include_dormant: true,
        include_expired,
        min_confidence,
        window: window(Some("2d"), None, TimeField::Created),
        ..Default::default()
    };
    queries::search_memories(store, &input)
        .unwrap()
        .into_iter()
        .map(|r| r.memory.id)
        .collect()
}

#[test]
fn limit_applies_after_expiry_so_a_valid_row_takes_the_slot() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let [recent, middle, _] = seeded(&store);
    testing::set_memory_column(&store, &recent, "valid_until", "2020-01-01T00:00:00Z").unwrap();

    assert_eq!(first_listed(&store, false, 0.0), vec![middle.clone()]);
    assert_eq!(
        first_listed(&store, true, 0.0),
        vec![middle],
        "expired is demoted below the valid row before the cut"
    );
}

#[test]
fn limit_applies_after_min_confidence() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let [recent, middle, _] = seeded(&store);
    testing::set_memory_column(&store, &recent, "confidence", 0.2).unwrap();

    assert_eq!(first_listed(&store, true, 0.5), vec![middle]);
}
