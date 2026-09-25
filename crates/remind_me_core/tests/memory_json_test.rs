//! The serialised `Memory` must cover every column of the `memories` table (#198).
//!
//! The reference builds its memory JSON as `dict(row)` over a `SELECT *`, so
//! its payload tracks the schema automatically. This crate uses a fixed
//! struct, which cannot — a column added to the schema simply never reaches a
//! response, silently, and no existing test noticed because they all compared
//! this crate against itself.
//!
//! Six columns had gone missing that way by the time anyone looked:
//! `memory_type`, `status`, `node_id`, `client`, `source_capture_id` and
//! `deleted_at`. `memory_type` was the sharp end — `remind_me_reclassify` set
//! a value that no client could read back.
//!
//! This file is the structural guard that replaces "someone notices". It
//! derives the expected key set from the live schema rather than restating it,
//! so it cannot itself drift out of date.

use remind_me_core::db::queries;
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::collections::BTreeSet;

/// Every column of `memories`, read from the database the crate actually opens.
fn schema_columns(conn: &Connection) -> BTreeSet<String> {
    let mut stmt = conn.prepare("PRAGMA table_info(memories)").unwrap();
    stmt.query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn memory_json_keys(conn: &Connection) -> BTreeSet<String> {
    let memory = queries::add_memory(
        conn,
        MemoryAddInput {
            content: "a memory to serialise".into(),
            category: "general".into(),
            tags: vec!["t".into()],
            source: "manual".into(),
            metadata: serde_json::json!({"k": "v"}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive: false,
        },
    )
    .unwrap();
    serde_json::to_value(&memory)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

#[test]
fn the_serialised_memory_covers_every_schema_column() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let columns = schema_columns(&conn);
    let keys = memory_json_keys(&conn);

    let missing: Vec<_> = columns.difference(&keys).cloned().collect();
    assert!(
        missing.is_empty(),
        "these `memories` columns are stored but never serialised, so no client \
         can see them: {missing:?}\n\
         Add them to `Memory` (models.rs), to MEMORY_COLUMNS and to \
         parse_memory_row (db/queries.rs)."
    );
}

#[test]
fn the_serialised_memory_invents_no_fields() {
    // The other direction. A key with no column behind it is a field a caller
    // could come to depend on that this crate cannot actually populate, and it
    // would put the two implementations out of step just as surely.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let columns = schema_columns(&conn);
    let keys = memory_json_keys(&conn);

    let extra: Vec<_> = keys.difference(&columns).cloned().collect();
    assert!(
        extra.is_empty(),
        "these serialised fields have no column behind them: {extra:?}"
    );
}

#[test]
fn the_six_fields_that_were_missing_are_present() {
    // Named explicitly as well as covered by the structural test above. If
    // someone ever relaxes the schema comparison, this still fails, and the
    // names are what makes the regression legible in a CI log.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let keys = memory_json_keys(&conn);

    for field in [
        "memory_type",
        "status",
        "node_id",
        "client",
        "source_capture_id",
        "deleted_at",
    ] {
        assert!(
            keys.contains(field),
            "{field} is missing from the memory JSON"
        );
    }
}

#[test]
fn memory_type_round_trips_through_the_json() {
    // The sharp end of #198. `reference` was added as an eighth memory_type,
    // and a client could not see any memory's type at all -- so this asserts
    // the value, not merely the key's presence.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let memory = queries::add_memory(
        &conn,
        MemoryAddInput {
            content: "standing reference material".into(),
            category: "general".into(),
            tags: vec![],
            source: "mempalace_import".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive: false,
        },
    )
    .unwrap();
    conn.execute(
        "UPDATE memories SET memory_type = 'reference' WHERE id = ?",
        [&memory.id],
    )
    .unwrap();

    let reread = queries::get_memory_by_id(&conn, &memory.id)
        .unwrap()
        .unwrap();
    let json = serde_json::to_value(&reread).unwrap();
    assert_eq!(
        json["memory_type"], "reference",
        "the classification must survive to the response"
    );
}
