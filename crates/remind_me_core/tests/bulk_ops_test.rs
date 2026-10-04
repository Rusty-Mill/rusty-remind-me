//! Coverage for `queries::bulk_delete` and `queries::bulk_tag`, the shared
//! implementations behind the HTTP-only `/api/memories/bulk/*` routes.

use remind_me_core::db::feedback::{Feedback, FeedbackEvent};
use remind_me_core::db::queries::{self, bulk_delete, bulk_tag};
use remind_me_core::db::Store;
use remind_me_core::testing::{self, Table};
use remind_me_core::{BulkTagInput, Database, MemoryAddInput, TagMode};

fn add(store: &Store<'_>, content: &str, tags: &[&str]) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            extract: true,
            attachments: vec![],
            sensitive: false,
            content: content.to_string(),
            category: "general".into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
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

fn tags_of(store: &Store<'_>, id: &str) -> Vec<String> {
    let raw = testing::memory_text(store, id, "tags").unwrap().unwrap();
    serde_json::from_str(&raw).unwrap()
}

// ---------------------------------------------------------------------------
// bulk_delete
// ---------------------------------------------------------------------------

#[test]
fn bulk_delete_removes_every_live_id() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "alpha", &[]);
    let b = add(&store, "beta", &[]);

    let result = bulk_delete(&store, &[a.clone(), b.clone()]).unwrap();

    assert_eq!(result.deleted, vec![a.clone(), b.clone()]);
    assert!(result.not_found.is_empty());
    assert_eq!(testing::count(&store, Table::Memories).unwrap(), 0);
}

#[test]
fn a_missing_id_does_not_fail_the_rest_of_the_batch() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "alpha", &[]);

    let result = bulk_delete(&store, &[a.clone(), "mem_ghost".to_string()]).unwrap();

    assert_eq!(result.deleted, vec![a]);
    assert_eq!(result.not_found, vec!["mem_ghost".to_string()]);
}

#[test]
fn bulk_delete_applies_the_same_per_memory_cleanup_as_delete_memory() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "alpha", &[]);
    Feedback::new(&store)
        .log_event(
            "fb1",
            &a,
            "query",
            &FeedbackEvent {
                query_tokens: "[]".into(),
                signal: "helpful".into(),
                magnitude: 0.1,
            },
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

    bulk_delete(&store, std::slice::from_ref(&a)).unwrap();

    // Reused delete_memory, not reimplemented — so its cleanup of
    // memory_feedback comes along for free.
    assert_eq!(Feedback::new(&store).events(&a).unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// bulk_tag
// ---------------------------------------------------------------------------

#[test]
fn add_mode_unions_onto_existing_tags() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "alpha", &["existing"]);

    let result = bulk_tag(
        &store,
        &BulkTagInput {
            ids: vec![a.clone()],
            tags: vec!["new".to_string(), "existing".to_string()],
            mode: TagMode::Add,
        },
    )
    .unwrap();

    assert_eq!(result.updated, vec![a.clone()]);
    assert_eq!(tags_of(&store, &a), vec!["existing", "new"]);
}

#[test]
fn remove_mode_drops_only_the_named_tags() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "alpha", &["keep", "drop-me"]);

    bulk_tag(
        &store,
        &BulkTagInput {
            ids: vec![a.clone()],
            tags: vec!["drop-me".to_string()],
            mode: TagMode::Remove,
        },
    )
    .unwrap();

    assert_eq!(tags_of(&store, &a), vec!["keep"]);
}

#[test]
fn set_mode_replaces_the_tags_wholesale() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "alpha", &["old-one", "old-two"]);

    bulk_tag(
        &store,
        &BulkTagInput {
            ids: vec![a.clone()],
            tags: vec!["fresh".to_string()],
            mode: TagMode::Set,
        },
    )
    .unwrap();

    assert_eq!(tags_of(&store, &a), vec!["fresh"]);
}

#[test]
fn set_mode_deduplicates_the_replacement_list() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "alpha", &[]);

    bulk_tag(
        &store,
        &BulkTagInput {
            ids: vec![a.clone()],
            tags: vec!["x".to_string(), "x".to_string()],
            mode: TagMode::Set,
        },
    )
    .unwrap();

    assert_eq!(tags_of(&store, &a), vec!["x"]);
}

#[test]
fn a_missing_id_is_reported_and_the_rest_still_applies() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let a = add(&store, "alpha", &[]);

    let result = bulk_tag(
        &store,
        &BulkTagInput {
            ids: vec![a.clone(), "mem_ghost".to_string()],
            tags: vec!["x".to_string()],
            mode: TagMode::Add,
        },
    )
    .unwrap();

    assert_eq!(result.updated, vec![a]);
    assert_eq!(result.not_found, vec!["mem_ghost".to_string()]);
}

#[test]
fn bulk_tag_default_mode_is_add() {
    assert_eq!(TagMode::default(), TagMode::Add);
}
