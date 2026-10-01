//! Coverage for saved and watched searches (gap T3, issue #108).
//!
//! The load-bearing distinction, and the one the issue's own wording gets
//! wrong: **running** a saved search returns all its current matches, watched
//! or not. Only **polling** reports the unseen ones. Both are pinned below,
//! because implementing it the other way round is the obvious guess.

use remind_me_core::db::queries;
use remind_me_core::db::saved_searches::SavedSearches;
use remind_me_core::db::Store;
use remind_me_core::saved_searches::{
    delete_saved_search, get_saved_search, list_saved_searches, poll_saved_search,
    poll_watched_searches, run_saved_search, save_search,
};
use remind_me_core::{Database, MemoryAddInput, SaveSearchInput, SavedSearch};

fn add(store: &Store<'_>, content: &str, category: &str, tags: &[&str]) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.to_string(),
            category: category.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive: false,
        },
    )
    .unwrap()
    .id
}

fn save(store: &Store<'_>, name: &str, query: &str, watch: bool) -> SavedSearch {
    save_search(
        store,
        &SaveSearchInput {
            name: name.to_string(),
            query: query.to_string(),
            category: None,
            tags: None,
            include_sensitive: false,
            watch,
        },
    )
    .unwrap()
}

fn seen_count(store: &Store<'_>, id: &str) -> usize {
    SavedSearches::new(store).seen_ids(id).unwrap().len()
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[test]
fn a_saved_search_round_trips_with_its_filters() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    let saved = save_search(
        &store,
        &SaveSearchInput {
            name: "quokkas".into(),
            query: "quokka".into(),
            category: Some("wildlife".into()),
            tags: Some(vec!["australia".into(), "photo".into()]),
            include_sensitive: true,
            watch: true,
        },
    )
    .unwrap();

    // The filters live in a JSON column, so this is really asserting that the
    // encode/decode pair agree — a mismatch there would silently drop filters
    // and quietly widen every re-run.
    let read_back = get_saved_search(&store, "quokkas").unwrap().unwrap();
    assert_eq!(read_back, saved);
    assert_eq!(read_back.filters.category.as_deref(), Some("wildlife"));
    assert_eq!(
        read_back.filters.tags,
        Some(vec!["australia".to_string(), "photo".to_string()])
    );
    assert!(read_back.filters.include_sensitive);
    assert!(read_back.watch);
}

#[test]
fn re_saving_a_name_updates_in_place_rather_than_duplicating() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    let first = save(&store, "recent", "postgres", false);
    let second = save(&store, "recent", "sqlite", true);

    // The table's UNIQUE on name means the alternative is an error, not a
    // second row — and re-saving is how a caller is meant to edit one.
    assert_eq!(first.id, second.id);
    assert_eq!(list_saved_searches(&store).unwrap().len(), 1);
    let current = get_saved_search(&store, "recent").unwrap().unwrap();
    assert_eq!(current.query, "sqlite");
    assert!(current.watch);
    assert_eq!(
        current.created_at, first.created_at,
        "an update must not rewrite when the search was first saved"
    );
}

#[test]
fn saved_searches_list_alphabetically() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    save(&store, "zebra", "z", false);
    save(&store, "alpha", "a", false);
    save(&store, "middle", "m", false);

    let names: Vec<String> = list_saved_searches(&store)
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();

    assert_eq!(names, vec!["alpha", "middle", "zebra"]);
}

#[test]
fn deleting_removes_the_search_and_its_seen_rows() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka sighting", "general", &[]);
    let saved = save(&store, "quokkas", "quokka", true);
    poll_saved_search(&store, &saved).unwrap();
    assert!(
        seen_count(&store, &saved.id) > 0,
        "the poll should have seeded"
    );

    assert!(delete_saved_search(&store, "quokkas").unwrap());

    assert!(get_saved_search(&store, "quokkas").unwrap().is_none());
    // Rows keyed by an id that no longer resolves are unreachable dead weight
    // — nothing will ever query them again.
    assert_eq!(seen_count(&store, &saved.id), 0);
}

#[test]
fn deleting_something_that_does_not_exist_is_false_not_an_error() {
    let db = Database::open_in_memory().unwrap();

    assert!(!delete_saved_search(&db.store(), "never-existed").unwrap());
}

// ---------------------------------------------------------------------------
// Running vs. polling — the distinction that matters
// ---------------------------------------------------------------------------

#[test]
fn running_a_watched_search_still_returns_every_match() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka on a beach", "general", &[]);
    add(&store, "quokka in a tree", "general", &[]);
    let saved = save(&store, "quokkas", "quokka", true);

    // Poll first, so everything is marked seen. If running narrowed to unseen
    // hits, this second run would come back empty.
    poll_saved_search(&store, &saved).unwrap();
    let results = run_saved_search(&store, &saved).unwrap();

    assert_eq!(
        results.len(),
        2,
        "running returns all matches; only polling diffs. A caller asking for a \
         saved search's results must not get a partial list because something \
         polled it earlier."
    );
}

#[test]
fn running_a_search_applies_its_stored_filters() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka sighting", "wildlife", &[]);
    add(&store, "quokka software release", "engineering", &[]);

    let saved = save_search(
        &store,
        &SaveSearchInput {
            name: "wild-quokkas".into(),
            query: "quokka".into(),
            category: Some("wildlife".into()),
            tags: None,
            include_sensitive: false,
            watch: false,
        },
    )
    .unwrap();

    let results = run_saved_search(&store, &saved).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].memory.category, "wildlife");
}

#[test]
fn the_first_poll_seeds_without_reporting_anything() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka one", "general", &[]);
    add(&store, "quokka two", "general", &[]);
    let saved = save(&store, "quokkas", "quokka", true);

    let outcome = poll_saved_search(&store, &saved).unwrap();

    // Turning on watch for a search that already matches is not the same as
    // those memories having just appeared. Reporting them would make enabling
    // a watch indistinguishable from a flood of new hits.
    assert!(outcome.seeded);
    assert!(outcome.new_matches.is_empty());
    assert_eq!(seen_count(&store, &saved.id), 2);
}

#[test]
fn a_later_poll_reports_only_matches_it_has_not_seen() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka one", "general", &[]);
    let saved = save(&store, "quokkas", "quokka", true);
    poll_saved_search(&store, &saved).unwrap();

    let fresh = add(&store, "quokka two", "general", &[]);
    let outcome = poll_saved_search(&store, &saved).unwrap();

    assert!(!outcome.seeded);
    assert_eq!(outcome.new_matches, vec![fresh]);
}

#[test]
fn a_match_is_reported_once_and_not_again() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka one", "general", &[]);
    let saved = save(&store, "quokkas", "quokka", true);
    poll_saved_search(&store, &saved).unwrap();
    add(&store, "quokka two", "general", &[]);

    assert_eq!(
        poll_saved_search(&store, &saved).unwrap().new_matches.len(),
        1
    );
    let third = poll_saved_search(&store, &saved).unwrap();

    // Without recording after reporting, every poll would re-report the same
    // match forever — the failure mode that makes a watch useless rather than
    // merely wrong.
    assert!(third.new_matches.is_empty());
}

#[test]
fn polling_covers_watched_searches_and_skips_the_rest() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "quokka sighting", "general", &[]);
    save(&store, "watched", "quokka", true);
    let unwatched = save(&store, "unwatched", "quokka", false);

    let outcomes = poll_watched_searches(&store).unwrap();

    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].0, "watched");
    assert_eq!(
        seen_count(&store, &unwatched.id),
        0,
        "an unwatched search must accumulate no tracking rows at all"
    );
}

#[test]
fn a_watched_search_with_no_matches_polls_cleanly() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let saved = save(&store, "nothing", "nonexistentterm", true);

    let first = poll_saved_search(&store, &saved).unwrap();
    let second = poll_saved_search(&store, &saved).unwrap();

    // An empty first poll leaves no seen rows, so the second is still a
    // seeding poll rather than reporting the first real match as "new" — which
    // is the correct reading: the watch has genuinely never seen anything yet.
    assert!(first.seeded);
    assert!(first.new_matches.is_empty());
    assert!(second.seeded);
}
