//! `remind_me_undo_import` on a **sync-enabled** node.
//!
//! Its own test binary rather than a case in `undo_import_test.rs`: the sync
//! switch is three process-wide env vars, and every other test in that file
//! asserts hard-delete behaviour. Sharing a process would mean either an
//! `ENV_LOCK` serialising the whole file or a race that shows up as a flake
//! months later.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::imports::ImportLedger;
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::outbox::Outbox;
use remind_me_core::db::Store;
use remind_me_core::sync::{HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV};
use remind_me_core::testing::{self, Table};
use remind_me_core::undo_import::undo_import;
use remind_me_core::{Database, UndoImportInput, UndoImportKind};

fn enable_sync() {
    crate::test_env::set_var(NODE_ID_ENV, "node-undo-test");
    crate::test_env::set_var(HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(SYNC_SECRET_ENV, "shh");
}

fn plant_chat_import(store: &Store<'_>, ids: &[&str], import_id: &str) {
    for id in ids {
        Memories::new(store)
            .insert(&NewMemory {
                source: "chat_import".into(),
                doc_id: Some(import_id.to_string()),
                chunk_index: Some(0),
                ..NewMemory::new(*id, format!("content {}", id), PLANTED_AT)
            })
            .unwrap();
    }
    ImportLedger::new(store)
        .record_chat(import_id, "chat.json", "h", PLANTED_AT, "{}")
        .unwrap();
    // Planted beside the importer, so make sure it is indexed: searchable,
    // and deletable without the full-text index losing track of it.
    remind_me_core::db::derived::rebuild_indexes(store).unwrap();
}

const PLANTED_AT: &str = "2026-01-01T00:00:00+00:00";

/// How many memories are tombstoned (`true`) or live (`false`).
fn tombstoned(store: &Store<'_>, deleted: bool) -> usize {
    testing::memory_ids(store)
        .unwrap()
        .iter()
        .filter(|id| {
            testing::memory_text(store, id, "deleted_at")
                .unwrap()
                .is_some()
                == deleted
        })
        .count()
}

#[test]
fn undo_tombstones_rather_than_deleting_when_sync_is_on() {
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    plant_chat_import(&store, &["mem_a", "mem_b"], "imp_1");
    Outbox::new(&store).clear().unwrap();

    let result = undo_import(
        &store,
        &UndoImportInput {
            import_kind: UndoImportKind::Chat,
            import_id: Some("imp_1".into()),
            dry_run: false,
            limit: 500,
        },
    )
    .unwrap();

    assert_eq!(result.removed, 2);
    assert!(
        result.mode.starts_with("soft-delete"),
        "the caller has to be told the space is not reclaimed yet, got {:?}",
        result.mode
    );

    // A hard delete produces no outbox row at all — the sync triggers only fire
    // on INSERT/UPDATE — so the removal would never propagate and the memories
    // would resurrect on the next pull from any peer that still has them.
    assert_eq!(
        tombstoned(&store, true),
        2,
        "rows must be tombstoned, not removed"
    );
    assert_eq!(tombstoned(&store, false), 0);

    let outbox = testing::outbox_rows(&store).unwrap();
    // The tombstone is an UPDATE that bumps updated_at, so it passes issue
    // #100's outbox guard and reaches peers.
    assert_eq!(
        outbox
            .iter()
            .filter(|row| row.operation == "update")
            .count(),
        2,
        "each tombstone must enqueue exactly one outbox row"
    );
    let payload_has_deleted_at = outbox
        .iter()
        .filter(|row| {
            let payload: serde_json::Value = serde_json::from_str(&row.payload).unwrap();
            !payload["deleted_at"].is_null()
        })
        .count();
    assert_eq!(
        payload_has_deleted_at, 2,
        "without deleted_at on the wire the peer cannot tell this was a deletion"
    );
}

#[test]
fn a_tombstoned_import_still_loses_its_tracking_row() {
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    plant_chat_import(&store, &["mem_a"], "imp_1");

    let result = undo_import(
        &store,
        &UndoImportInput {
            import_kind: UndoImportKind::Chat,
            import_id: Some("imp_1".into()),
            dry_run: false,
            limit: 500,
        },
    )
    .unwrap();

    // The surviving-chunks check keys on `deleted_at IS NULL`, so a tombstoned
    // row must not count as surviving. If it did, the tracking row would stay
    // forever on a sync-enabled node and the file could never be re-imported —
    // a bug that would only ever appear on synced installs.
    assert_eq!(result.tracking_rows_removed, 1);
    assert_eq!(testing::count(&store, Table::ChatImports).unwrap(), 0);
}
