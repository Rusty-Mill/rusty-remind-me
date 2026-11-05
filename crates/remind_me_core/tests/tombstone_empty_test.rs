//! Tombstones are emptied, not purged (ADR-0024).
//!
//! A deleted memory keeps what last-write-wins and re-imports read and drops
//! its text: on delete, and at open for the ones deleted before that. Its
//! revision history goes with it. Only a store that synced tombstones
//! (`testing::mark_store_syncing`); one that never did hard deletes.

use remind_me_core::db::history::Revisions;
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::models::{MemoryAddInput, MemoryUpdateInput};
use remind_me_core::sync::TOMBSTONE_CONTENT;
use remind_me_core::testing::{self, Table};
use remind_me_core::Database;
use serde_json::json;
use std::path::PathBuf;

/// A fresh directory for one store, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rrm_tombstone_{tag}_{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("memory.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A memory with text in every column a tombstone drops, and one revision.
fn add_with_history(store: &Store<'_>) -> String {
    let id = queries::add_memory(
        store,
        MemoryAddInput {
            sensitive: false,
            content: "the secret plan".to_string(),
            category: "general".into(),
            tags: vec!["private".into()],
            source: "chat_import".into(),
            metadata: json!({ "chat_id": "c1" }),
            subject: Some("Ada".into()),
            predicate: Some("plans".into()),
            object: Some("a secret".into()),
            entities: vec![],
            ..Default::default()
        },
    )
    .unwrap()
    .id;
    queries::update_memory(
        store,
        &serde_json::from_value::<MemoryUpdateInput>(json!({
            "memory_id": id,
            "content": "the secret plan, revised",
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(Revisions::new(store).list(&id, 10).unwrap().len(), 1);
    id
}

fn text(store: &Store<'_>, id: &str, column: &str) -> Option<String> {
    testing::memory_text(store, id, column).unwrap()
}

/// Assert `id` is a tombstone holding none of its text.
fn assert_emptied(store: &Store<'_>, id: &str) {
    assert!(text(store, id, "deleted_at").is_some(), "still a tombstone");
    assert_eq!(
        text(store, id, "content").as_deref(),
        Some(TOMBSTONE_CONTENT)
    );
    assert_eq!(text(store, id, "tags").as_deref(), Some("[]"));
    for column in ["subject", "predicate", "object"] {
        assert_eq!(text(store, id, column), None, "{column}");
    }
    // What LWW and a re-import read stays.
    assert_eq!(text(store, id, "source").as_deref(), Some("chat_import"));
    assert!(text(store, id, "metadata").unwrap().contains("c1"));
}

#[test]
fn a_delete_drops_the_text_and_the_history() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    testing::mark_store_syncing(&store).unwrap();
    let id = add_with_history(&store);

    assert!(queries::delete_memory(&store, &id).unwrap());

    assert_emptied(&store, &id);
    assert!(Revisions::new(&store).list(&id, 10).unwrap().is_empty());
    // The delete it queues carries no text either.
    let queued = testing::outbox_rows(&store).unwrap();
    let payload = &queued.last().unwrap().payload;
    assert!(payload.contains(TOMBSTONE_CONTENT), "{payload}");
    assert!(!payload.contains("secret"), "{payload}");
}

#[test]
fn a_hard_delete_drops_the_history_too() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add_with_history(&store);

    assert!(queries::delete_memory(&store, &id).unwrap());
    assert!(
        text(&store, &id, "id").is_none(),
        "a store that never synced hard deletes"
    );
    assert!(Revisions::new(&store).list(&id, 10).unwrap().is_empty());
}

#[test]
fn opening_empties_tombstones_deleted_before_and_queues_nothing() {
    let scratch = Scratch::new("open");
    let (id, queued) = {
        let db = Database::open(scratch.db()).unwrap();
        let store = db.store();
        testing::mark_store_syncing(&store).unwrap();
        let id = add_with_history(&store);
        // A tombstone as an earlier build left it: text, tags and history.
        assert!(queries::delete_memory(&store, &id).unwrap());
        testing::set_memory_column(&store, &id, "content", "the secret plan").unwrap();
        testing::set_memory_column(&store, &id, "tags", "[\"private\"]").unwrap();
        testing::set_memory_column(&store, &id, "subject", "Ada").unwrap();
        // The raw writes above bypass SQLite's full-text index; an earlier
        // build's tombstone had its text indexed, so rebuild to match.
        remind_me_core::db::derived::rebuild_indexes(&store).unwrap();
        let updated_at = text(&store, &id, "updated_at");
        (
            id,
            (
                testing::count(&store, Table::SyncOutbox).unwrap(),
                updated_at,
            ),
        )
    };

    let db = Database::open(scratch.db()).unwrap();
    let store = db.store();
    assert_emptied(&store, &id);
    assert_eq!(
        (
            testing::count(&store, Table::SyncOutbox).unwrap(),
            text(&store, &id, "updated_at")
        ),
        queued,
        "emptying is storage, not an edit: nothing queued, updated_at as it was"
    );
    assert_eq!(
        queries::empty_tombstones(&store).unwrap(),
        (0, 0),
        "idempotent"
    );
}

#[test]
fn opening_drops_the_history_of_memories_deleted_before() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    testing::mark_store_syncing(&store).unwrap();
    let kept = add_with_history(&store);
    let gone = add_with_history(&store);
    // A tombstone whose revisions an earlier build left behind.
    testing::set_memory_column(&store, &gone, "deleted_at", "2026-08-05T00:00:00+00:00").unwrap();

    let (_, revisions) = queries::empty_tombstones(&store).unwrap();
    assert_eq!(revisions, 1);
    assert!(Revisions::new(&store).list(&gone, 10).unwrap().is_empty());
    assert_eq!(
        Revisions::new(&store).list(&kept, 10).unwrap().len(),
        1,
        "a live memory keeps its history"
    );
}
