//! `remind_me_update`'s `clear_superseded` flag (issue #174).
//!
//! The recovery path for a false-positive contradiction-supersession: a reused
//! generic `(subject, predicate)` pair can supersede an unrelated memory, and
//! without this there is no way to un-hide it.

use remind_me_core::db::queries;
use remind_me_core::{
    Database, MemoryAddInput, MemorySearchInput, MemoryUpdateInput, UpdateOutcome,
};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str, triple: Option<(&str, &str, &str)>) -> String {
    let (subject, predicate, object) = match triple {
        Some((s, p, o)) => (
            Some(s.to_string()),
            Some(p.to_string()),
            Some(o.to_string()),
        ),
        None => (None, None, None),
    };
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: "general".to_string(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject,
            predicate,
            object,
            entities: vec![],
            sensitive: false,
        },
    )
    .expect("add")
    .id
}

fn update(conn: &Connection, id: &str, clear_superseded: bool) -> UpdateOutcome {
    queries::update_memory(
        conn,
        &MemoryUpdateInput {
            memory_id: id.to_string(),
            content: None,
            category: None,
            tags: None,
            metadata: None,
            sensitive: None,
            clear_superseded,
        },
    )
    .expect("update")
}

fn superseded_by(conn: &Connection, id: &str) -> Option<String> {
    conn.query_row(
        "SELECT superseded_by FROM memories WHERE id = ?",
        [id],
        |r| r.get(0),
    )
    .expect("row")
}

/// Build the situation this flag exists to recover from: two memories sharing
/// a `(subject, predicate)` with different objects, the older one superseded.
///
/// Supersession is driven explicitly by `entity::supersede_contradicting_facts`
/// rather than as a side effect of `add_memory`, so the fixture calls it the
/// same way the real add path does. Returns `(superseded, superseding)`.
fn supersede(conn: &Connection) -> (String, String) {
    let first = add(
        conn,
        "the deploy target is staging",
        Some(("deploy", "target", "staging")),
    );
    let second = add(
        conn,
        "the deploy target is production",
        Some(("deploy", "target", "production")),
    );
    let hit = remind_me_core::entity::supersede_contradicting_facts(
        conn,
        &second,
        Some("deploy"),
        Some("target"),
        Some("production"),
    )
    .expect("supersede");
    assert_eq!(
        hit,
        vec![first.clone()],
        "fixture precondition: the older fact should have been superseded"
    );
    (first, second)
}

#[test]
fn clearing_unhides_a_superseded_memory() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    let (first, _) = supersede(&conn);

    assert!(matches!(
        update(&conn, &first, true),
        UpdateOutcome::Updated(_)
    ));
    assert_eq!(
        superseded_by(&conn, &first),
        None,
        "clear_superseded should null the pointer"
    );
}

#[test]
fn omitting_the_flag_leaves_the_pointer_alone() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    let (first, second) = supersede(&conn);

    // A content edit that says nothing about supersession must not un-hide it.
    queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            memory_id: first.clone(),
            content: Some("the deploy target is staging (revised)".to_string()),
            category: None,
            tags: None,
            metadata: None,
            sensitive: None,
            clear_superseded: false,
        },
    )
    .expect("update");

    assert_eq!(
        superseded_by(&conn, &first),
        Some(second),
        "an unrelated edit must not clear the supersession"
    );
}

/// The reference is explicit that this "does not affect the memory that did
/// the superseding" — clearing must not cascade.
#[test]
fn clearing_does_not_touch_the_superseding_memory() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    let (first, second) = supersede(&conn);

    update(&conn, &first, true);

    assert_eq!(
        superseded_by(&conn, &second),
        None,
        "the superseding memory was never superseded itself"
    );
    // And it is still present and readable.
    assert!(queries::get_memory_by_id(&conn, &second)
        .expect("get")
        .is_some());
}

/// The point of the flag: search filters on `superseded_by IS NULL`, so
/// clearing it is what actually brings the memory back.
#[test]
fn a_cleared_memory_is_searchable_again() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    let (first, _) = supersede(&conn);

    let search = |conn: &Connection| {
        queries::search_memories(
            conn,
            &MemorySearchInput {
                query: "staging".to_string(),
                limit: 20,
                ..Default::default()
            },
        )
        .expect("search")
        .iter()
        .any(|r| r.memory.id == first)
    };

    assert!(!search(&conn), "a superseded memory should not surface");
    update(&conn, &first, true);
    assert!(search(&conn), "clearing should bring it back to search");
}

/// `clear_superseded` alone is a real update, not "no fields provided".
#[test]
fn the_flag_alone_counts_as_an_update() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    let (first, _) = supersede(&conn);

    assert!(
        matches!(update(&conn, &first, true), UpdateOutcome::Updated(_)),
        "the flag on its own must not be reported as NoFields"
    );
}

/// An update naming no fields at all is still `NoFields` — the flag defaulting
/// to false must not turn every empty update into a write.
#[test]
fn an_empty_update_is_still_no_fields() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    let id = add(&conn, "a standalone note", None);

    assert!(matches!(update(&conn, &id, false), UpdateOutcome::NoFields));
}

/// Clearing a memory that was never superseded is a no-op, not an error.
#[test]
fn clearing_an_unsuperseded_memory_is_harmless() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    let id = add(&conn, "a standalone note", None);

    assert!(matches!(
        update(&conn, &id, true),
        UpdateOutcome::Updated(_)
    ));
    assert_eq!(superseded_by(&conn, &id), None);
}

/// Deserialization contract: a caller that omits the field gets `false`, and
/// one that sends it gets what they sent. This is what a real MCP payload
/// exercises, so it is worth asserting directly.
#[test]
fn the_field_defaults_to_false_when_absent_from_json() {
    let without: MemoryUpdateInput =
        serde_json::from_value(serde_json::json!({ "memory_id": "m1" })).expect("parse");
    assert!(!without.clear_superseded);

    let with: MemoryUpdateInput =
        serde_json::from_value(serde_json::json!({ "memory_id": "m1", "clear_superseded": true }))
            .expect("parse");
    assert!(with.clear_superseded);
}
