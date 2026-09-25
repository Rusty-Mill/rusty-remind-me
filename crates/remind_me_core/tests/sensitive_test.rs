//! Coverage for the sensitive-memory flag (gap T11, issue #105).
//!
//! **This is not access control.** The reference is explicit that this is a
//! single-user store: anyone who can read the database file reads every
//! memory in it, marked or not. The flag is a "don't surface by default"
//! convenience — it keeps a memory out of ordinary search and list results so
//! it does not appear over someone's shoulder, and nothing more. These tests
//! assert surfacing behaviour, and deliberately do not assert anything that
//! would read as a confidentiality guarantee.

use remind_me_core::db::queries;
use remind_me_core::{
    Database, MemoryAddInput, MemoryListInput, MemorySearchInput, MemoryUpdateInput, UpdateOutcome,
};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str, sensitive: bool) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive,
        },
    )
    .unwrap()
    .id
}

fn search(conn: &Connection, query: &str, include_sensitive: bool) -> Vec<String> {
    queries::search_memories(
        conn,
        &MemorySearchInput {
            strategy: Default::default(),
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
            include_sensitive,
        },
    )
    .unwrap()
    .into_iter()
    .map(|r| r.memory.id)
    .collect()
}

fn list(conn: &Connection, include_sensitive: bool) -> (usize, Vec<String>) {
    let result = queries::list_memories(
        conn,
        &MemoryListInput {
            category: None,
            tags: None,
            source: None,
            limit: 100,
            offset: 0,
            response_format: Default::default(),
            include_sensitive,
        },
    )
    .unwrap();
    (
        result.total,
        result.memories.into_iter().map(|m| m.id).collect(),
    )
}

fn stored_flag(conn: &Connection, id: &str) -> i64 {
    conn.query_row("SELECT sensitive FROM memories WHERE id = ?", [id], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn a_memory_is_not_sensitive_by_default() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting", false);

    // The whole feature is additive with a false default, so an existing
    // caller that never heard of the flag must see no behaviour change.
    assert_eq!(stored_flag(&conn, &id), 0);
    assert_eq!(search(&conn, "quokka", false), vec![id.clone()]);
    assert_eq!(list(&conn, false).1, vec![id]);
}

#[test]
fn a_sensitive_memory_is_excluded_from_search_by_default() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let ordinary = add(&conn, "quokka sighting at the beach", false);
    let secret = add(&conn, "quokka sighting, undisclosed location", true);

    assert_eq!(search(&conn, "quokka", false), vec![ordinary.clone()]);

    let mut both = search(&conn, "quokka", true);
    both.sort();
    let mut want = vec![ordinary, secret];
    want.sort();
    assert_eq!(both, want, "include_sensitive must bring it back");
}

#[test]
fn a_sensitive_memory_is_excluded_from_list_by_default() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let ordinary = add(&conn, "ordinary", false);
    add(&conn, "hidden", true);

    let (total, ids) = list(&conn, false);

    // `total` matters as much as the page: the count is a SQL condition rather
    // than a post-filter precisely so COUNT, LIMIT and OFFSET agree. A total of
    // 2 alongside one row would make pagination skip a page.
    assert_eq!(ids, vec![ordinary]);
    assert_eq!(total, 1, "the excluded row must not be counted either");

    assert_eq!(list(&conn, true).0, 2);
}

#[test]
fn the_flag_can_be_set_and_cleared_after_creation() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting", false);

    let update = |sensitive: Option<bool>| {
        queries::update_memory(
            &conn,
            &MemoryUpdateInput {
                memory_id: id.clone(),
                clear_superseded: false,
                content: None,
                category: None,
                tags: None,
                metadata: None,
                sensitive,
            },
        )
        .unwrap()
    };

    assert!(matches!(update(Some(true)), UpdateOutcome::Updated(_)));
    assert_eq!(stored_flag(&conn, &id), 1);
    assert!(search(&conn, "quokka", false).is_empty());

    assert!(matches!(update(Some(false)), UpdateOutcome::Updated(_)));
    assert_eq!(stored_flag(&conn, &id), 0);
    assert_eq!(search(&conn, "quokka", false), vec![id]);
}

#[test]
fn an_update_that_does_not_mention_the_flag_leaves_it_alone() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting", true);

    queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            memory_id: id.clone(),
            clear_superseded: false,
            content: Some("quokka sighting, confirmed".into()),
            category: None,
            tags: None,
            metadata: None,
            sensitive: None,
        },
    )
    .unwrap();

    // This is why the field is `Option<bool>` and not `bool`. With two states,
    // every content edit would silently unhide the memory — the failure would
    // be invisible until something surfaced that should not have.
    assert_eq!(stored_flag(&conn, &id), 1);
    assert!(search(&conn, "quokka", false).is_empty());
}

#[test]
fn an_update_of_only_the_flag_is_not_reported_as_no_fields() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting", false);

    let outcome = queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            memory_id: id,
            clear_superseded: false,
            content: None,
            category: None,
            tags: None,
            metadata: None,
            sensitive: Some(true),
        },
    )
    .unwrap();

    // `NoFields` exists to report "you asked for nothing". Marking a memory
    // sensitive is something, so it must not fall into that branch — which it
    // would if the flag were appended after the emptiness check.
    assert!(matches!(outcome, UpdateOutcome::Updated(_)));
}

#[test]
fn the_flag_survives_a_round_trip_through_the_input_json() {
    // The MCP surface deserialises these from JSON, so the serde defaults are
    // part of the contract: an old caller sending neither field must land on
    // false rather than failing to parse.
    let add: MemoryAddInput =
        serde_json::from_value(serde_json::json!({ "content": "x" })).unwrap();
    assert!(!add.sensitive);

    let add: MemoryAddInput =
        serde_json::from_value(serde_json::json!({ "content": "x", "sensitive": true })).unwrap();
    assert!(add.sensitive);

    let search: MemorySearchInput =
        serde_json::from_value(serde_json::json!({ "query": "x" })).unwrap();
    assert!(!search.include_sensitive);

    let list: MemoryListInput = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(!list.include_sensitive);

    let update: MemoryUpdateInput =
        serde_json::from_value(serde_json::json!({ "memory_id": "m" })).unwrap();
    assert_eq!(update.sensitive, None, "absent must mean 'leave it alone'");
}
