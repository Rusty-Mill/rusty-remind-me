//! `Database::open` on the engine for an on-disk node (ADR-0023 §5): the
//! first open copies the SQLite file, later opens use the copy, and the
//! file is refused without the engine once its store has moved.

use super::copy::copy_into_place;
use super::{retry_while_locked, TemporaryDir};
use crate::db::memories::{Memories, NewMemory};
use crate::db::{engine_dir, Database, StoreError};
use crate::testing::{self, Table};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const T1: &str = "2026-01-01T00:00:00+00:00";

/// A node's SQLite file holding one memory, in a directory of its own.
fn sqlite_node(dir: &TemporaryDir) -> PathBuf {
    std::fs::create_dir_all(dir.path()).unwrap();
    let file = dir.path().join("memory.db");
    let db = Database::open_on_sqlite(&file).unwrap();
    Memories::new(&db.store())
        .insert(&NewMemory::new("m1", "copied from SQLite", T1))
        .unwrap();
    file
}

fn memories(db: &Database) -> i64 {
    testing::count(&db.store(), Table::Memories).unwrap()
}

/// The memory rows in the SQLite file itself, bypassing the engine.
fn sqlite_memories(file: &Path) -> i64 {
    let conn = Connection::open(file).unwrap();
    conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
        .unwrap()
}

fn open_on_engine(file: &Path) -> Database {
    retry_while_locked(|| Database::open_on_engine(file)).unwrap()
}

#[test]
fn the_first_open_copies_the_file_and_later_opens_keep_the_engine() {
    let dir = TemporaryDir::fresh();
    let file = sqlite_node(&dir);
    {
        let db = open_on_engine(&file);
        assert!(engine_dir(&file).is_dir());
        assert_eq!(memories(&db), 1);
        Memories::new(&db.store())
            .insert(&NewMemory::new("m2", "written on the engine", T1))
            .unwrap();
    }
    let db = open_on_engine(&file);
    assert_eq!(memories(&db), 2);
    // The file stopped changing at the copy.
    assert_eq!(sqlite_memories(&file), 1);
}

#[test]
fn the_file_is_refused_without_the_engine_once_copied() {
    let dir = TemporaryDir::fresh();
    let file = sqlite_node(&dir);
    drop(open_on_engine(&file));
    let refused = Database::open_on_sqlite(&file);
    let why = refused.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(why.contains("copied onto the engine"), "{why}");
}

#[test]
fn a_partial_copy_left_by_a_crash_is_redone() {
    let dir = TemporaryDir::fresh();
    let file = sqlite_node(&dir);
    let partial = dir.path().join("memory.engine.partial");
    std::fs::create_dir_all(&partial).unwrap();
    std::fs::write(partial.join("stray"), b"half a copy").unwrap();
    let db = open_on_engine(&file);
    assert_eq!(memories(&db), 1);
    assert!(!partial.exists());
    assert!(!engine_dir(&file).join("stray").exists());
}

#[test]
fn a_refused_row_leaves_no_engine_store() {
    let dir = TemporaryDir::fresh();
    let file = sqlite_node(&dir);
    {
        // A column the engine has nowhere to keep: the copy would lose it.
        // (Opening the file with this build rebuilds the table without it,
        // so the copy is called directly.)
        let conn = Connection::open(&file).unwrap();
        conn.execute_batch(
            "ALTER TABLE memories ADD COLUMN extra TEXT;
             UPDATE memories SET extra = 'kept only by SQLite';",
        )
        .unwrap();
    }
    let engine = engine_dir(&file);
    let refused = copy_into_place(&file, &engine);
    assert!(
        matches!(refused, Err(StoreError::Invalid(ref why)) if why.contains("could not be copied")),
        "{refused:?}"
    );
    assert!(!engine.exists());
    assert!(dir.path().join("memory.engine.partial").is_dir());
    assert_eq!(sqlite_memories(&file), 1);
}

#[test]
fn background_threads_share_the_engine_store() {
    let dir = TemporaryDir::fresh();
    let file = sqlite_node(&dir);
    let db = open_on_engine(&file);
    let source = db.secondary_source().unwrap();
    let conn = Database::open_secondary_at(source.path()).unwrap();
    Memories::new(&source.store(&conn))
        .insert(&NewMemory::new("m2", "from a background thread", T1))
        .unwrap();
    assert_eq!(memories(&db), 2);
    assert_eq!(sqlite_memories(&file), 1);
}
