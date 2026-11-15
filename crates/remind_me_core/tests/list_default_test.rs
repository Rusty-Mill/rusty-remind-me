//! #223: a derived `Default` ignored `#[serde(default = ...)]`, so
//! `MemoryListInput::default()` had `limit: 0`, which `list_memories` clamped
//! to 1 and a Rust caller silently got one row. `ExportInput` had the same
//! shape with `include_graph`.

use remind_me_core::db::{queries, Store, StoreError};
use remind_me_core::models::{ExportInput, MemoryAddInput, MemoryListInput, LIST_LIMIT_MAX};
use remind_me_core::Database;

fn seed(store: &Store<'_>, n: usize) {
    for i in 0..n {
        queries::add_memory(
            store,
            MemoryAddInput {
                content: format!("seed {i}"),
                ..Default::default()
            },
        )
        .unwrap();
    }
}

#[test]
fn list_default_matches_the_wire_default() {
    let wire: MemoryListInput = serde_json::from_str("{}").unwrap();
    assert_eq!(MemoryListInput::default().limit, 20);
    assert_eq!(MemoryListInput::default().limit, wire.limit);
}

#[test]
fn list_with_default_input_returns_more_than_one_row() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    seed(&store, 3);

    let page = queries::list_memories(&store, &MemoryListInput::default()).unwrap();

    assert_eq!(page.count, 3);
    assert_eq!(page.limit, 20);
}

#[test]
fn list_refuses_an_explicit_zero_limit() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    seed(&store, 2);
    let input = MemoryListInput {
        limit: 0,
        ..Default::default()
    };

    let err = queries::list_memories(&store, &input).unwrap_err();

    assert!(
        matches!(&err, StoreError::Invalid(why) if why.contains("limit")),
        "{err}"
    );
}

#[test]
fn list_still_clamps_a_limit_above_the_maximum() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let input = MemoryListInput {
        limit: LIST_LIMIT_MAX + 1,
        ..Default::default()
    };

    let page = queries::list_memories(&store, &input).unwrap();

    assert_eq!(page.limit, LIST_LIMIT_MAX);
}

#[test]
fn export_default_includes_the_graph_like_the_wire_default() {
    let wire: ExportInput = serde_json::from_str("{}").unwrap();
    assert!(ExportInput::default().include_graph);
    assert_eq!(ExportInput::default().include_graph, wire.include_graph);
    assert!(!ExportInput::default().include_deleted);
}
