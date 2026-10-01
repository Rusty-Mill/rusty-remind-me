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

use remind_me_core::db::derived::Origin;
use remind_me_core::db::entities::Entities;
use remind_me_core::db::feedback::{Feedback, FeedbackEvent};
use remind_me_core::db::imports::ImportLedger;
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::Store;
use remind_me_core::entity::Entity;
use remind_me_core::testing::{self, Table};
use remind_me_core::undo_import::undo_import;
use remind_me_core::{Database, UndoImportInput, UndoImportKind, UndoImportResult};

const PLANTED_AT: &str = "2026-01-01T00:00:00+00:00";

fn plant_memory(store: &Store<'_>, id: &str, source: &str, doc_id: Option<&str>, metadata: &str) {
    Memories::new(store)
        .insert(&NewMemory {
            source: source.to_string(),
            metadata: serde_json::from_str(metadata).unwrap(),
            doc_id: doc_id.map(str::to_string),
            chunk_index: Some(0),
            ..NewMemory::new(id, format!("content {}", id), PLANTED_AT)
        })
        .unwrap();
    // Planted beside the importers, so index it the way a rebuild would, to
    // be searchable and deletable without the full-text index losing track.
    remind_me_core::db::derived::rebuild_indexes(store).unwrap();
}

/// Record chat import `import_id` in the ledger, as `import_chat` does.
fn plant_chat_import(store: &Store<'_>, import_id: &str) {
    ImportLedger::new(store)
        .record_chat(import_id, "chat.json", "h", PLANTED_AT, "{}")
        .unwrap();
}

fn live_ids(store: &Store<'_>) -> Vec<String> {
    testing::memory_ids(store)
        .unwrap()
        .into_iter()
        .filter(|id| {
            testing::memory_text(store, id, "deleted_at")
                .unwrap()
                .is_none()
        })
        .collect()
}

fn count(store: &Store<'_>, table: Table) -> i64 {
    testing::count(store, table).unwrap()
}

fn run(store: &Store<'_>, kind: UndoImportKind, id: Option<&str>, dry: bool) -> UndoImportResult {
    undo_import(
        store,
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
    let store = db.store();
    plant_memory(&store, "mem_a", "chat_import", Some("imp_1"), "{}");
    plant_memory(&store, "mem_b", "chat_import", Some("imp_1"), "{}");
    plant_chat_import(&store, "imp_1");

    let result = run(&store, UndoImportKind::Chat, Some("imp_1"), true);

    assert!(result.dry_run);
    assert_eq!(result.matched, 2);
    assert_eq!(result.removed, 0);
    assert_eq!(result.remaining, 2);
    assert!(result.hint.is_some(), "a dry run must say how to commit it");
    assert_eq!(live_ids(&store).len(), 2, "a dry run must change nothing");
    assert_eq!(count(&store, Table::ChatImports), 1);
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
    let store = db.store();
    plant_memory(&store, "mem_a", "chat_import", Some("imp_1"), "{}");
    plant_memory(&store, "mem_b", "chat_import", Some("imp_1"), "{}");
    plant_memory(&store, "mem_other", "manual", None, "{}");
    plant_chat_import(&store, "imp_1");

    let result = run(&store, UndoImportKind::Chat, Some("imp_1"), false);

    assert_eq!(result.removed, 2);
    assert_eq!(result.remaining, 0);
    assert_eq!(result.tracking_rows_removed, 1);
    assert_eq!(
        live_ids(&store),
        vec!["mem_other"],
        "an unrelated manual memory must survive"
    );
    // The tracking row has to go, or the same file can never be imported again:
    // every import path treats a tracked id as already done.
    assert_eq!(count(&store, Table::ChatImports), 0);
}

#[test]
fn a_dbs_import_round_trips_and_scopes_by_source_prefix() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for (id, source) in [
        ("mem_x", "notion"),
        ("mem_y", "notion"),
        ("mem_z", "linear"),
    ] {
        plant_memory(&store, id, "dbs_import", None, "{}");
        ImportLedger::new(&store)
            .record_dbs(source, id, id, "h", PLANTED_AT)
            .unwrap();
    }

    let result = run(&store, UndoImportKind::Dbs, Some("notion"), false);

    assert_eq!(result.removed, 2);
    assert_eq!(result.tracking_rows_removed, 2);
    assert_eq!(live_ids(&store), vec!["mem_z"]);
    assert_eq!(count(&store, Table::DbsImports), 1);
}

#[test]
fn a_mempalace_undo_covers_untracked_content_too() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // Written through the tracked path.
    plant_memory(
        &store,
        "mem_tracked",
        "mempalace_import",
        None,
        r#"{"mempalace_drawer_id": "wing_a/drawer_1"}"#,
    );
    ImportLedger::new(&store)
        .record_mempalace("wing_a/drawer_1", "mem_tracked", PLANTED_AT)
        .unwrap();
    // Mempalace content that never got a tracking row — a bulk load predating
    // the ledger. Unambiguously mempalace by source and metadata.
    plant_memory(
        &store,
        "mem_untracked",
        "mempalace:obsidian",
        None,
        r#"{"mempalace_drawer_id": "wing_a/drawer_2"}"#,
    );
    plant_memory(&store, "mem_other", "manual", None, "{}");

    let result = run(&store, UndoImportKind::Mempalace, Some("wing_a"), false);

    // Trusting the tracking table alone would silently leave half the batch
    // behind — and leave it looking like the undo succeeded.
    assert_eq!(result.matched, 2);
    assert_eq!(result.removed, 2);
    assert_eq!(live_ids(&store), vec!["mem_other"]);
}

#[test]
fn an_unscoped_undo_takes_every_record_of_that_kind() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for (id, source) in [("mem_x", "notion"), ("mem_y", "linear")] {
        plant_memory(&store, id, "dbs_import", None, "{}");
        ImportLedger::new(&store)
            .record_dbs(source, id, id, "h", PLANTED_AT)
            .unwrap();
    }
    plant_memory(&store, "mem_manual", "manual", None, "{}");

    let result = run(&store, UndoImportKind::Dbs, None, false);

    assert_eq!(result.removed, 2);
    assert_eq!(result.scope, "all dbs imports");
    assert_eq!(live_ids(&store), vec!["mem_manual"]);
}

// ---------------------------------------------------------------------------
// Partial overlap, resumability, and the awkward cases
// ---------------------------------------------------------------------------

#[test]
fn a_partially_drained_chat_import_keeps_its_tracking_row() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for id in ["mem_a", "mem_b", "mem_c"] {
        plant_memory(&store, id, "chat_import", Some("imp_1"), "{}");
    }
    plant_chat_import(&store, "imp_1");

    let first = undo_import(
        &store,
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
    assert_eq!(count(&store, Table::ChatImports), 1);

    let second = run(&store, UndoImportKind::Chat, Some("imp_1"), false);

    assert_eq!(second.removed, 1);
    assert_eq!(second.remaining, 0);
    assert_eq!(second.tracking_rows_removed, 1);
    assert_eq!(count(&store, Table::ChatImports), 0);
}

#[test]
fn an_edited_imported_memory_is_still_removed() {
    // The issue's partial-overlap case: a memory arrived by import and was
    // edited afterwards. Editing does not detach it from the import — doc_id
    // is untouched by an update — so an undo of that import still claims it.
    // Worth pinning because the opposite behaviour is defensible-sounding and
    // would leave orphans that no undo can ever reach.
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    plant_memory(&store, "mem_edited", "chat_import", Some("imp_1"), "{}");
    plant_chat_import(&store, "imp_1");
    remind_me_core::db::queries::update_memory(
        &store,
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

    let result = run(&store, UndoImportKind::Chat, Some("imp_1"), false);

    assert_eq!(result.removed, 1);
    assert!(live_ids(&store).is_empty());
}

#[test]
fn an_unknown_import_id_is_an_empty_result_not_an_error() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    plant_memory(&store, "mem_a", "chat_import", Some("imp_1"), "{}");

    let result = run(&store, UndoImportKind::Chat, Some("imp_nonexistent"), false);

    assert_eq!(result.matched, 0);
    assert_eq!(result.removed, 0);
    assert_eq!(result.remaining, 0);
    assert_eq!(
        live_ids(&store),
        vec!["mem_a"],
        "nothing else may be touched"
    );
}

#[test]
fn undoing_twice_is_harmless() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    plant_memory(&store, "mem_a", "chat_import", Some("imp_1"), "{}");
    plant_chat_import(&store, "imp_1");

    assert_eq!(
        run(&store, UndoImportKind::Chat, Some("imp_1"), false).removed,
        1
    );
    let again = run(&store, UndoImportKind::Chat, Some("imp_1"), false);

    // Resumability means re-running is expected, so a second pass over an
    // already-emptied import has to be a no-op rather than an error.
    assert_eq!(again.matched, 0);
    assert_eq!(again.removed, 0);
}

#[test]
fn related_rows_go_with_the_memory() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    plant_memory(&store, "mem_a", "chat_import", Some("imp_1"), "{}");
    let entities = Entities::new(&store);
    entities
        .insert(
            &Entity {
                id: "ent_1".into(),
                name: "thing".into(),
                kind: Some("concept".into()),
                aliases: Vec::new(),
                created_at: PLANTED_AT.into(),
                updated_at: PLANTED_AT.into(),
            },
            None,
        )
        .unwrap();
    entities
        .link("mem_a", "ent_1", PLANTED_AT, Origin::Local)
        .unwrap();
    Feedback::new(&store)
        .log_event(
            "fb_1",
            "mem_a",
            "q",
            &FeedbackEvent {
                query_tokens: "[\"q\"]".into(),
                signal: "helpful".into(),
                magnitude: 0.1,
            },
            PLANTED_AT,
        )
        .unwrap();

    run(&store, UndoImportKind::Chat, Some("imp_1"), false);

    // Routing through delete_memory rather than a bulk DELETE is what buys
    // this. Orphaned vec_chunks in particular are actively dangerous: SQLite
    // reuses freed rowids, so a later memory could inherit these vectors.
    assert_eq!(count(&store, Table::MemoryEntities), 0);
    assert_eq!(count(&store, Table::MemoryFeedback), 0);
    // The entity itself survives — other memories may still mention it.
    assert_eq!(count(&store, Table::Entities), 1);
}
