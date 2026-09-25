//! Coverage for `remind_me_undo_import`.
//!
//! Rows are planted directly rather than driven through the importers: the
//! three ledgers have different shapes and this is about the *undo* resolving
//! each of them, not about re-testing the importers that fill them.
//!
//! Sync is left unconfigured throughout except in the tombstone test, so the
//! default here is a hard delete — which is also the harsher case to assert
//! against, since a hard delete genuinely removes the row a soft delete would
//! leave behind for the tracking query to trip over.

use remind_me_core::undo_import::undo_import;
use remind_me_core::{Database, UndoImportInput, UndoImportKind, UndoImportResult};
use rusqlite::Connection;

fn plant_memory(conn: &Connection, id: &str, source: &str, doc_id: Option<&str>, metadata: &str) {
    conn.execute(
        "INSERT INTO memories (id, content, category, tags, source, metadata,
                               created_at, updated_at, doc_id, chunk_index)
         VALUES (?, ?, 'general', '[]', ?, ?, '2026-01-01T00:00:00+00:00',
                 '2026-01-01T00:00:00+00:00', ?, 0)",
        rusqlite::params![id, format!("content {}", id), source, metadata, doc_id],
    )
    .unwrap();
}

fn live_ids(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT id FROM memories WHERE deleted_at IS NULL ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {}", table), [], |r| r.get(0))
        .unwrap()
}

fn run(conn: &Connection, kind: UndoImportKind, id: Option<&str>, dry: bool) -> UndoImportResult {
    undo_import(
        conn,
        &UndoImportInput {
            import_kind: kind,
            import_id: id.map(str::to_string),
            dry_run: dry,
            limit: 500,
        },
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Dry run
// ---------------------------------------------------------------------------

#[test]
fn a_dry_run_reports_without_removing_anything() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant_memory(&conn, "mem_a", "chat_import", Some("imp_1"), "{}");
    plant_memory(&conn, "mem_b", "chat_import", Some("imp_1"), "{}");
    conn.execute(
        "INSERT INTO chat_imports (import_id, filename, hash, imported_at)
         VALUES ('imp_1', 'chat.json', 'h', '2026-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();

    let result = run(&conn, UndoImportKind::Chat, Some("imp_1"), true);

    assert!(result.dry_run);
    assert_eq!(result.matched, 2);
    assert_eq!(result.removed, 0);
    assert_eq!(result.remaining, 2);
    assert!(result.hint.is_some(), "a dry run must say how to commit it");
    assert_eq!(live_ids(&conn).len(), 2, "a dry run must change nothing");
    assert_eq!(count(&conn, "chat_imports"), 1);
}

#[test]
fn dry_run_is_the_default() {
    // The field default is the safety property, so it is worth pinning
    // separately from the behaviour: a future `#[serde(default)]` slip would
    // turn every unspecified call into a bulk delete.
    let input: UndoImportInput =
        serde_json::from_value(serde_json::json!({ "import_kind": "chat" })).unwrap();

    assert!(input.dry_run);
    assert_eq!(input.limit, 500);
    assert!(input.import_id.is_none());
}

// ---------------------------------------------------------------------------
// The three ledgers
// ---------------------------------------------------------------------------

#[test]
fn a_chat_import_round_trips() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant_memory(&conn, "mem_a", "chat_import", Some("imp_1"), "{}");
    plant_memory(&conn, "mem_b", "chat_import", Some("imp_1"), "{}");
    plant_memory(&conn, "mem_other", "manual", None, "{}");
    conn.execute(
        "INSERT INTO chat_imports (import_id, filename, hash, imported_at)
         VALUES ('imp_1', 'chat.json', 'h', '2026-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();

    let result = run(&conn, UndoImportKind::Chat, Some("imp_1"), false);

    assert_eq!(result.removed, 2);
    assert_eq!(result.remaining, 0);
    assert_eq!(result.tracking_rows_removed, 1);
    assert_eq!(
        live_ids(&conn),
        vec!["mem_other"],
        "an unrelated manual memory must survive"
    );
    // The tracking row has to go, or the same file can never be imported again:
    // every import path treats a tracked id as already done.
    assert_eq!(count(&conn, "chat_imports"), 0);
}

#[test]
fn a_dbs_import_round_trips_and_scopes_by_source_prefix() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for (id, source) in [
        ("mem_x", "notion"),
        ("mem_y", "notion"),
        ("mem_z", "linear"),
    ] {
        plant_memory(&conn, id, "dbs_import", None, "{}");
        conn.execute(
            "INSERT INTO dbs_imports (dbs_source, external_id, memory_id, content_hash, imported_at)
             VALUES (?, ?, ?, 'h', '2026-01-01T00:00:00+00:00')",
            rusqlite::params![source, id, id],
        )
        .unwrap();
    }

    let result = run(&conn, UndoImportKind::Dbs, Some("notion"), false);

    assert_eq!(result.removed, 2);
    assert_eq!(result.tracking_rows_removed, 2);
    assert_eq!(live_ids(&conn), vec!["mem_z"]);
    assert_eq!(count(&conn, "dbs_imports"), 1);
}

#[test]
fn a_mempalace_undo_covers_untracked_content_too() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Written through the tracked path.
    plant_memory(
        &conn,
        "mem_tracked",
        "mempalace_import",
        None,
        r#"{"mempalace_drawer_id": "wing_a/drawer_1"}"#,
    );
    conn.execute(
        "INSERT INTO mempalace_imports (drawer_id, memory_id, imported_at)
         VALUES ('wing_a/drawer_1', 'mem_tracked', '2026-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();
    // Mempalace content that never got a tracking row — a bulk load predating
    // the ledger. Unambiguously mempalace by source and metadata.
    plant_memory(
        &conn,
        "mem_untracked",
        "mempalace:obsidian",
        None,
        r#"{"mempalace_drawer_id": "wing_a/drawer_2"}"#,
    );
    plant_memory(&conn, "mem_other", "manual", None, "{}");

    let result = run(&conn, UndoImportKind::Mempalace, Some("wing_a"), false);

    // Trusting the tracking table alone would silently leave half the batch
    // behind — and leave it looking like the undo succeeded.
    assert_eq!(result.matched, 2);
    assert_eq!(result.removed, 2);
    assert_eq!(live_ids(&conn), vec!["mem_other"]);
}

#[test]
fn an_unscoped_undo_takes_every_record_of_that_kind() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for (id, source) in [("mem_x", "notion"), ("mem_y", "linear")] {
        plant_memory(&conn, id, "dbs_import", None, "{}");
        conn.execute(
            "INSERT INTO dbs_imports (dbs_source, external_id, memory_id, content_hash, imported_at)
             VALUES (?, ?, ?, 'h', '2026-01-01T00:00:00+00:00')",
            rusqlite::params![source, id, id],
        )
        .unwrap();
    }
    plant_memory(&conn, "mem_manual", "manual", None, "{}");

    let result = run(&conn, UndoImportKind::Dbs, None, false);

    assert_eq!(result.removed, 2);
    assert_eq!(result.scope, "all dbs imports");
    assert_eq!(live_ids(&conn), vec!["mem_manual"]);
}

// ---------------------------------------------------------------------------
// Partial overlap, resumability, and the awkward cases
// ---------------------------------------------------------------------------

#[test]
fn a_partially_drained_chat_import_keeps_its_tracking_row() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for id in ["mem_a", "mem_b", "mem_c"] {
        plant_memory(&conn, id, "chat_import", Some("imp_1"), "{}");
    }
    conn.execute(
        "INSERT INTO chat_imports (import_id, filename, hash, imported_at)
         VALUES ('imp_1', 'chat.json', 'h', '2026-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();

    let first = undo_import(
        &conn,
        &UndoImportInput {
            import_kind: UndoImportKind::Chat,
            import_id: Some("imp_1".into()),
            dry_run: false,
            limit: 2,
        },
    )
    .unwrap();

    assert_eq!(first.removed, 2);
    assert_eq!(first.remaining, 1);
    assert!(first.hint.is_some(), "an unfinished undo must say so");
    // Dropping the tracking row now would let a re-import duplicate the chunk
    // that is still here. The row survives until nothing of the import does.
    assert_eq!(
        first.tracking_rows_removed, 0,
        "a partially-drained import keeps its tracking row"
    );
    assert_eq!(count(&conn, "chat_imports"), 1);

    let second = run(&conn, UndoImportKind::Chat, Some("imp_1"), false);

    assert_eq!(second.removed, 1);
    assert_eq!(second.remaining, 0);
    assert_eq!(second.tracking_rows_removed, 1);
    assert_eq!(count(&conn, "chat_imports"), 0);
}

#[test]
fn an_edited_imported_memory_is_still_removed() {
    // The issue's partial-overlap case: a memory arrived by import and was
    // edited afterwards. Editing does not detach it from the import — doc_id
    // is untouched by an update — so an undo of that import still claims it.
    // Worth pinning because the opposite behaviour is defensible-sounding and
    // would leave orphans that no undo can ever reach.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant_memory(&conn, "mem_edited", "chat_import", Some("imp_1"), "{}");
    conn.execute(
        "INSERT INTO chat_imports (import_id, filename, hash, imported_at)
         VALUES ('imp_1', 'chat.json', 'h', '2026-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();
    remind_me_core::db::queries::update_memory(
        &conn,
        &remind_me_core::MemoryUpdateInput {
            sensitive: None,
            memory_id: "mem_edited".into(),
            content: Some("hand-edited afterwards".into()),
            category: None,
            tags: None,
            metadata: None,
            clear_superseded: false,
        },
    )
    .unwrap();

    let result = run(&conn, UndoImportKind::Chat, Some("imp_1"), false);

    assert_eq!(result.removed, 1);
    assert!(live_ids(&conn).is_empty());
}

#[test]
fn an_unknown_import_id_is_an_empty_result_not_an_error() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant_memory(&conn, "mem_a", "chat_import", Some("imp_1"), "{}");

    let result = run(&conn, UndoImportKind::Chat, Some("imp_nonexistent"), false);

    assert_eq!(result.matched, 0);
    assert_eq!(result.removed, 0);
    assert_eq!(result.remaining, 0);
    assert_eq!(
        live_ids(&conn),
        vec!["mem_a"],
        "nothing else may be touched"
    );
}

#[test]
fn undoing_twice_is_harmless() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant_memory(&conn, "mem_a", "chat_import", Some("imp_1"), "{}");
    conn.execute(
        "INSERT INTO chat_imports (import_id, filename, hash, imported_at)
         VALUES ('imp_1', 'chat.json', 'h', '2026-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();

    assert_eq!(
        run(&conn, UndoImportKind::Chat, Some("imp_1"), false).removed,
        1
    );
    let again = run(&conn, UndoImportKind::Chat, Some("imp_1"), false);

    // Resumability means re-running is expected, so a second pass over an
    // already-emptied import has to be a no-op rather than an error.
    assert_eq!(again.matched, 0);
    assert_eq!(again.removed, 0);
}

#[test]
fn related_rows_go_with_the_memory() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant_memory(&conn, "mem_a", "chat_import", Some("imp_1"), "{}");
    conn.execute(
        "INSERT INTO entities (id, name, kind, aliases, created_at, updated_at)
         VALUES ('ent_1', 'thing', 'concept', '[]', '2026-01-01T00:00:00+00:00',
                 '2026-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_entities (memory_id, entity_id, created_at)
         VALUES ('mem_a', 'ent_1', '2026-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_feedback
             (id, memory_id, query, query_tokens, signal, magnitude, created_at)
         VALUES ('fb_1', 'mem_a', 'q', '[\"q\"]', 'helpful', 0.1,
                 '2026-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();

    run(&conn, UndoImportKind::Chat, Some("imp_1"), false);

    // Routing through delete_memory rather than a bulk DELETE is what buys
    // this. Orphaned vec_chunks in particular are actively dangerous: SQLite
    // reuses freed rowids, so a later memory could inherit these vectors.
    assert_eq!(count(&conn, "memory_entities"), 0);
    assert_eq!(count(&conn, "memory_feedback"), 0);
    // The entity itself survives — other memories may still mention it.
    assert_eq!(count(&conn, "entities"), 1);
}
