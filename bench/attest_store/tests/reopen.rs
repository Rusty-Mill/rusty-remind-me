//! Writes through a database on the Attest candidate survive a reopen,
//! which rebuilds the engine tables from the log alone.

use remind_me_core::db::history::Revisions;
use remind_me_core::db::{queries, StoreError};
use remind_me_core::models::MemoryAddInput;
use remind_me_core::{testing, Database};
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("attest_store_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn add(db: &Database, content: &str) -> String {
    let input = MemoryAddInput {
        content: content.into(),
        category: "fact".into(),
        ..Default::default()
    };
    queries::add_memory(&db.store(), input).unwrap().id
}

fn open(dir: &Path) -> Database {
    let tables = attest_store::open(&dir.join("log")).unwrap();
    Database::open_backend(Box::new(tables), Some(dir.join("memory.db"))).unwrap()
}

#[test]
fn writes_survive_a_reopen_from_the_log() {
    let dir = scratch("reopen");
    let (kept, gone) = {
        let db = open(&dir);
        let kept = add(&db, "quokkas are marsupials");
        let gone = add(&db, "deleted before the reopen");
        assert!(queries::delete_memory(&db.store(), &gone).unwrap());
        (kept, gone)
    };
    let db = open(&dir);
    let store = db.store();
    assert_eq!(
        testing::memory_text(&store, &kept, "content")
            .unwrap()
            .as_deref(),
        Some("quokkas are marsupials")
    );
    assert!(testing::memory_text(&store, &gone, "id").unwrap().is_none());
    let input = serde_json::from_value(serde_json::json!({ "query": "quokkas" })).unwrap();
    let found = queries::search_memories_with_embedder(&store, &input, None).unwrap();
    assert_eq!(
        found.len(),
        1,
        "the full-text index was rebuilt from the log"
    );
    drop(store);
    let again = add(&db, "written after the reopen");
    drop(db);
    let db = open(&dir);
    assert!(testing::memory_text(&db.store(), &again, "id")
        .unwrap()
        .is_some());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_second_opener_is_refused() {
    let dir = scratch("locked");
    let _first = open(&dir);
    assert!(attest_store::open(&dir.join("log")).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

const NOW: &str = "2026-11-22T00:00:00+00:00";

/// A transaction that aborts after writing a revision and a row change
/// leaves neither in the log: the reopened store has the row as it was, no
/// revision, and does not issue the aborted revision's id again.
#[test]
fn an_aborted_mixed_edit_leaves_nothing_in_the_log() {
    let dir = scratch("aborted");
    let (id, issued) = {
        let db = open(&dir);
        let id = add(&db, "before the edit");
        let store = db.store();
        let mut issued = 0;
        let aborted: Result<(), StoreError> = store.transaction(|s| {
            let revisions = Revisions::new(s);
            let current = revisions.current(&id)?.expect("live");
            revisions.insert(&id, &current, NOW, None)?;
            issued = revisions.list(&id, 1)?[0].id;
            let edit = serde_json::from_value(
                serde_json::json!({ "memory_id": id, "content": "after the edit" }),
            )
            .unwrap();
            queries::update_memory(s, &edit).map_err(|e| StoreError::Engine(e.to_string()))?;
            Err(StoreError::Engine("abort".into()))
        });
        assert!(aborted.is_err());
        (id, issued)
    };
    let db = open(&dir);
    let store = db.store();
    assert_eq!(
        testing::memory_text(&store, &id, "content")
            .unwrap()
            .as_deref(),
        Some("before the edit")
    );
    let revisions = Revisions::new(&store);
    assert!(revisions.list(&id, 10).unwrap().is_empty());
    // Reopened straight after the rollback: its issued id is not reused.
    let current = revisions.current(&id).unwrap().expect("live");
    revisions.insert(&id, &current, NOW, None).unwrap();
    let next = revisions.list(&id, 1).unwrap()[0].id;
    assert!(next > issued, "{next} reissues {issued} or lower");
    drop(store);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Deleting the newest revision, then reopening from the log alone, does
/// not let its id be issued again.
#[test]
fn a_reopen_never_reissues_a_deleted_revision_id() {
    let dir = scratch("sequence");
    let (id, top) = {
        let db = open(&dir);
        let id = add(&db, "tracked");
        let store = db.store();
        let revisions = Revisions::new(&store);
        let current = revisions.current(&id).unwrap().expect("live");
        revisions.insert(&id, &current, NOW, None).unwrap();
        let top = revisions.list(&id, 1).unwrap()[0].id;
        revisions.delete_for(&id).unwrap();
        (id, top)
    };
    let db = open(&dir);
    let store = db.store();
    let revisions = Revisions::new(&store);
    let current = revisions.current(&id).unwrap().expect("live");
    revisions.insert(&id, &current, NOW, None).unwrap();
    let next = revisions.list(&id, 1).unwrap()[0].id;
    assert!(next > top, "{next} reissues {top} or lower");
    drop(store);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

fn open_at(log: &Path) -> Database {
    let tables = attest_store::open_at(log, &log.with_extension("temp")).unwrap();
    Database::open_backend(Box::new(tables), None).unwrap()
}

/// The engine tables live where `open_at` puts them, and are rebuilt
/// there from the log at every open. A second opener of the same log is
/// refused before it clears the first one's tables.
#[test]
fn open_at_keeps_the_tables_beside_the_log_and_rebuilds_them() {
    let dir = scratch("open_at");
    let log = dir.join("store");
    let id = {
        let db = open_at(&log);
        let id = add(&db, "kept beside the log");
        assert!(
            std::fs::read_dir(log.with_extension("temp"))
                .unwrap()
                .count()
                > 0
        );
        assert!(attest_store::open_at(&log, &log.with_extension("temp")).is_err());
        assert!(testing::memory_text(&db.store(), &id, "id")
            .unwrap()
            .is_some());
        id
    };
    let db = open_at(&log);
    assert!(testing::memory_text(&db.store(), &id, "id")
        .unwrap()
        .is_some());
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A package is the log alone, without the lock or the engine tables,
/// and opens anywhere as the same store.
#[test]
fn a_package_restores_from_the_log_alone() {
    let dir = scratch("package");
    let (log, package) = (dir.join("store"), dir.join("package"));
    let db = open_at(&log);
    let id = add(&db, "packaged");
    attest_store::package(&log, &package).unwrap();
    assert!(!package.join("LOCK").exists());
    drop(db);
    std::fs::rename(&log, dir.join("moved")).unwrap();
    let restored = open_at(&package);
    assert_eq!(
        testing::memory_text(&restored.store(), &id, "content")
            .unwrap()
            .as_deref(),
        Some("packaged")
    );
    drop(restored);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An engine path that exists but was not made by `open_at` is refused
/// and left as it was.
#[test]
fn open_at_refuses_to_clear_a_directory_it_did_not_make() {
    let dir = scratch("foreign");
    let foreign = dir.join("someone-elses");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("keep.txt"), b"precious").unwrap();
    assert!(attest_store::open_at(&dir.join("store"), &foreign).is_err());
    assert_eq!(
        std::fs::read(foreign.join("keep.txt")).unwrap(),
        b"precious"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Scratch another run's log made, mark and all, is not this log's to clear.
#[test]
fn open_at_refuses_another_logs_marked_scratch() {
    let dir = scratch("other-run");
    let engine = dir.join("engine");
    drop(attest_store::open_at(&dir.join("first"), &engine).unwrap());
    std::fs::write(engine.join("keep.txt"), b"precious").unwrap();
    assert!(attest_store::open_at(&dir.join("second"), &engine).is_err());
    assert_eq!(std::fs::read(engine.join("keep.txt")).unwrap(), b"precious");
    drop(attest_store::open_at(&dir.join("first"), &engine).unwrap());
    assert!(
        !engine.join("keep.txt").exists(),
        "its own log may rebuild it"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
