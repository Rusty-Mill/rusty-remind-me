//! `remind_me_undo_import` on a **sync-enabled** node.
//!
//! Its own test binary rather than a case in `undo_import_test.rs`: the sync
//! switch is three process-wide env vars, and every other test in that file
//! asserts hard-delete behaviour. Sharing a process would mean either an
//! `ENV_LOCK` serialising the whole file or a race that shows up as a flake
//! months later.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::sync::{HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV};
use remind_me_core::undo_import::undo_import;
use remind_me_core::{Database, UndoImportInput, UndoImportKind};
use rusqlite::Connection;

fn enable_sync() {
    crate::test_env::set_var(NODE_ID_ENV, "node-undo-test");
    crate::test_env::set_var(HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(SYNC_SECRET_ENV, "shh");
}

fn plant_chat_import(conn: &Connection, ids: &[&str], import_id: &str) {
    for id in ids {
        conn.execute(
            "INSERT INTO memories (id, content, category, tags, source, metadata,
                                   created_at, updated_at, doc_id, chunk_index)
             VALUES (?, ?, 'general', '[]', 'chat_import', '{}',
                     '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00', ?, 0)",
            rusqlite::params![id, format!("content {}", id), import_id],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO chat_imports (import_id, filename, hash, imported_at)
         VALUES (?, 'chat.json', 'h', '2026-01-01T00:00:00+00:00')",
        rusqlite::params![import_id],
    )
    .unwrap();
}

fn scalar(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn undo_tombstones_rather_than_deleting_when_sync_is_on() {
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant_chat_import(&conn, &["mem_a", "mem_b"], "imp_1");
    conn.execute("DELETE FROM sync_outbox", []).unwrap();

    let result = undo_import(
        &conn,
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
        scalar(
            &conn,
            "SELECT count(*) FROM memories WHERE deleted_at IS NOT NULL"
        ),
        2,
        "rows must be tombstoned, not removed"
    );
    assert_eq!(
        scalar(
            &conn,
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL"
        ),
        0
    );

    // The tombstone is an UPDATE that bumps updated_at, so it passes issue
    // #100's outbox guard and reaches peers.
    assert_eq!(
        scalar(
            &conn,
            "SELECT count(*) FROM sync_outbox WHERE operation = 'update'"
        ),
        2,
        "each tombstone must enqueue exactly one outbox row"
    );
    let payload_has_deleted_at = scalar(
        &conn,
        "SELECT count(*) FROM sync_outbox
          WHERE json_extract(payload, '$.deleted_at') IS NOT NULL",
    );
    assert_eq!(
        payload_has_deleted_at, 2,
        "without deleted_at on the wire the peer cannot tell this was a deletion"
    );
}

#[test]
fn a_tombstoned_import_still_loses_its_tracking_row() {
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    plant_chat_import(&conn, &["mem_a"], "imp_1");

    let result = undo_import(
        &conn,
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
    assert_eq!(scalar(&conn, "SELECT count(*) FROM chat_imports"), 0);
}
