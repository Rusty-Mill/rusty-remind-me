//! Export excludes deleted and superseded memories by default (issue #175).
//!
//! This is a data-integrity test, not a filter test. Every exported record
//! carries `role: "assistant"` so the importer reads it back as live content —
//! so an export that included tombstones and superseded facts would resurrect
//! them as fresh live memories on the next round-trip. That is the failure
//! these assertions exist to prevent.

use remind_me_core::db::queries;
use remind_me_core::{entity, export, Database, ExportInput, MemoryAddInput};
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

fn export_input(include_deleted: bool) -> ExportInput {
    ExportInput {
        format: Default::default(),
        category: None,
        tags: None,
        file_path: None,
        include_graph: false,
        include_deleted,
    }
}

fn exported_contents(conn: &Connection, include_deleted: bool) -> Vec<String> {
    export::collect_export_records(conn, &export_input(include_deleted))
        .expect("export")
        .iter()
        .filter_map(|r| {
            r.get("content")
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// Supersede an older fact, returning the superseded memory's content.
fn supersede(conn: &Connection) -> &'static str {
    add(
        conn,
        "deploy target is staging",
        Some(("deploy", "target", "staging")),
    );
    let newer = add(
        conn,
        "deploy target is production",
        Some(("deploy", "target", "production")),
    );
    let hit = entity::supersede_contradicting_facts(
        conn,
        &newer,
        Some("deploy"),
        Some("target"),
        Some("production"),
    )
    .expect("supersede");
    assert_eq!(
        hit.len(),
        1,
        "fixture: exactly one fact should be superseded"
    );
    "deploy target is staging"
}

#[test]
fn a_superseded_memory_is_excluded_by_default() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    let stale = supersede(&conn);

    let contents = exported_contents(&conn, false);
    assert!(
        !contents.iter().any(|c| c == stale),
        "a superseded memory must not be exported by default -- re-importing \
         it would resurrect it as live content. Got: {:?}",
        contents
    );
    assert!(
        contents.iter().any(|c| c == "deploy target is production"),
        "the live fact should still be exported"
    );
}

#[test]
fn include_deleted_brings_the_superseded_memory_back() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    let stale = supersede(&conn);

    let contents = exported_contents(&conn, true);
    assert!(
        contents.iter().any(|c| c == stale),
        "include_deleted is the audit/full-backup escape hatch and must \
         include superseded memories. Got: {:?}",
        contents
    );
    assert_eq!(contents.len(), 2, "both facts should be present");
}

/// The reference gates *both* conditions on this one flag. Tombstones are the
/// other half, and they only exist when sync is on -- `delete_memory`
/// hard-deletes otherwise.
#[test]
fn a_tombstoned_memory_is_excluded_by_default() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    let id = add(&conn, "a note that gets deleted", None);
    add(&conn, "a note that survives", None);

    // Tombstone directly rather than via `delete_memory`, so this test does not
    // depend on sync being configured in the test environment.
    conn.execute(
        "UPDATE memories SET deleted_at = ? WHERE id = ?",
        rusqlite::params!["2026-01-01T00:00:00Z", id],
    )
    .expect("tombstone");

    let contents = exported_contents(&conn, false);
    assert!(
        !contents.iter().any(|c| c == "a note that gets deleted"),
        "a tombstoned memory must not be exported by default. Got: {:?}",
        contents
    );
    assert_eq!(contents, vec!["a note that survives".to_string()]);

    let with = exported_contents(&conn, true);
    assert_eq!(
        with.len(),
        2,
        "include_deleted should bring the tombstone back"
    );
}

/// Both conditions are gated together, so a vault carrying one of each must
/// lose both by default — checking only `deleted_at` would pass a test that
/// used a tombstone alone.
#[test]
fn both_exclusions_apply_together() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    let stale = supersede(&conn);
    let doomed = add(&conn, "a note that gets deleted", None);
    conn.execute(
        "UPDATE memories SET deleted_at = ? WHERE id = ?",
        rusqlite::params!["2026-01-01T00:00:00Z", doomed],
    )
    .expect("tombstone");

    let contents = exported_contents(&conn, false);
    assert_eq!(
        contents,
        vec!["deploy target is production".to_string()],
        "only the one live memory should survive both exclusions"
    );
    assert!(!contents.iter().any(|c| c == stale));

    assert_eq!(
        exported_contents(&conn, true).len(),
        3,
        "include_deleted should return all three"
    );
}

/// The default must not quietly drop live memories along with the dead ones.
#[test]
fn ordinary_memories_are_unaffected() {
    let db = Database::open(":memory:").expect("db");
    let conn = db.conn();
    for note in ["first", "second", "third"] {
        add(&conn, note, None);
    }
    assert_eq!(exported_contents(&conn, false).len(), 3);
    assert_eq!(exported_contents(&conn, true).len(), 3);
}

/// Deserialization contract: absent means false, which is what makes the safe
/// default actually reach a real MCP payload.
#[test]
fn the_field_defaults_to_false_when_absent_from_json() {
    let without: ExportInput = serde_json::from_value(serde_json::json!({})).expect("parse");
    assert!(!without.include_deleted);
    assert!(
        without.include_graph,
        "include_graph's own default must survive this addition"
    );

    let with: ExportInput =
        serde_json::from_value(serde_json::json!({ "include_deleted": true })).expect("parse");
    assert!(with.include_deleted);
}
