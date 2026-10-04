//! `Database::open` for an on-disk node (ADR-0023 §5, ADR-0025): the first
//! open copies an old SQLite file beside which no engine directory exists,
//! later opens use the copy, and the file is never opened again.

use super::copy::copy_into_place;
use super::{retry_while_locked, TemporaryDir};
use crate::db::legacy_sqlite::{fixture, LegacyDb};
use crate::db::memories::{Memories, NewMemory};
use crate::db::{engine_dir, Database, StoreError};
use crate::testing::{self, Table};
use std::path::{Path, PathBuf};

const T1: &str = "2026-01-01T00:00:00+00:00";

/// A node's old SQLite file holding three memories (the v32 fixture), in a
/// directory of its own.
fn sqlite_node(dir: &TemporaryDir) -> PathBuf {
    std::fs::create_dir_all(dir.path()).unwrap();
    let file = dir.path().join("memory.db");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy_store/legacy_v32.db"),
        &file,
    )
    .unwrap();
    file
}

fn memories(db: &Database) -> i64 {
    testing::count(&db.store(), Table::Memories).unwrap()
}

/// The memory rows in the SQLite file itself, bypassing the engine.
fn sqlite_memories(file: &Path) -> usize {
    LegacyDb::open(file)
        .unwrap()
        .rows("memories")
        .unwrap()
        .len()
}

fn open(file: &Path) -> Database {
    retry_while_locked(|| Database::open(file)).unwrap()
}

#[test]
fn the_first_open_copies_the_file_and_later_opens_keep_the_engine() {
    let dir = TemporaryDir::fresh();
    let file = sqlite_node(&dir);
    {
        let db = open(&file);
        assert!(engine_dir(&file).is_dir());
        assert_eq!(memories(&db), 3);
        Memories::new(&db.store())
            .insert(&NewMemory::new("m4", "written on the engine", T1))
            .unwrap();
    }
    let db = open(&file);
    assert_eq!(memories(&db), 4);
    // The file stopped changing at the copy.
    assert_eq!(sqlite_memories(&file), 3);
}

#[test]
fn a_node_with_no_file_starts_empty_on_the_engine() {
    let dir = TemporaryDir::fresh();
    std::fs::create_dir_all(dir.path()).unwrap();
    let file = dir.path().join("memory.db");
    let db = open(&file);
    assert_eq!(memories(&db), 0);
    assert!(engine_dir(&file).is_dir());
    assert!(!file.exists(), "no SQLite file is ever created");
    assert_eq!(db.store().path(), Some(file.as_path()));
}

#[test]
fn a_partial_copy_left_by_a_crash_is_redone() {
    let dir = TemporaryDir::fresh();
    let file = sqlite_node(&dir);
    let partial = dir.path().join("memory.engine.partial");
    std::fs::create_dir_all(&partial).unwrap();
    std::fs::write(partial.join("stray"), b"half a copy").unwrap();
    let db = open(&file);
    assert_eq!(memories(&db), 3);
    assert!(!partial.exists());
    assert!(!engine_dir(&file).join("stray").exists());
}

#[test]
fn a_refused_row_leaves_no_engine_store() {
    let dir = TemporaryDir::fresh();
    let file = sqlite_node(&dir);
    // A column the engine has nowhere to keep: the copy would lose it.
    fixture(
        &file,
        "ALTER TABLE memories ADD COLUMN extra TEXT;
         UPDATE memories SET extra = 'kept only by SQLite';",
    )
    .unwrap();
    let engine = engine_dir(&file);
    let refused = copy_into_place(&file, &engine, &mut |_| {});
    assert!(
        matches!(refused, Err(StoreError::Invalid(ref why)) if why.contains("could not be copied")),
        "{refused:?}"
    );
    assert!(!engine.exists());
    assert!(dir.path().join("memory.engine.partial").is_dir());
    assert_eq!(sqlite_memories(&file), 3);
    // `Database::open` reports the same and leaves the file as it was.
    let opened = Database::open(&file).map(|_| ());
    assert!(matches!(opened, Err(StoreError::Invalid(_))), "{opened:?}");
    assert_eq!(sqlite_memories(&file), 3);
}

#[test]
fn a_file_the_copy_cannot_read_is_refused_with_the_remedy() {
    let dir = TemporaryDir::fresh();
    let file = sqlite_node(&dir);
    fixture(&file, "PRAGMA user_version = 29;").unwrap();
    let opened = Database::open(&file);
    let why = opened.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        why.contains("schema version 29") && why.contains("0.2.x"),
        "{why}"
    );
    assert!(!engine_dir(&file).exists());
}

#[test]
fn background_threads_share_the_engine_store() {
    let dir = TemporaryDir::fresh();
    let file = sqlite_node(&dir);
    let db = open(&file);
    let source = db.secondary_source().unwrap();
    assert_eq!(source.path(), file.as_path());
    Memories::new(&source.store())
        .insert(&NewMemory::new("m4", "from a background thread", T1))
        .unwrap();
    assert_eq!(memories(&db), 4);
    assert_eq!(sqlite_memories(&file), 3);
}
