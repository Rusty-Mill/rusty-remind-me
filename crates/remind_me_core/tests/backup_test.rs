//! Coverage for `remind_me_backup`.

use remind_me_core::backup::{
    backup_dir, create_backup, list_backups, BackupError, BACKUP_RETENTION_COUNT,
};
use remind_me_core::db::queries;
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// A scratch directory that cleans itself up.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        // Thread id keeps parallel tests from sharing a directory.
        let unique = format!(
            "rmm_backup_{}_{}_{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        );
        let path = std::env::temp_dir().join(unique.replace(['(', ')', ' '], ""));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn db_path(&self) -> PathBuf {
        self.0.join("remind_me.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn add(conn: &Connection, content: &str) {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: "general".to_string(),
            tags: vec![],
            source: "manual".to_string(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap();
}

fn count_memories(path: &Path) -> i64 {
    let conn = Connection::open(path).unwrap();
    conn.query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn backup_of_an_in_memory_database_is_refused_clearly() {
    let db = Database::open_in_memory().unwrap();
    let err = create_backup(&db.conn(), "manual").unwrap_err();

    assert!(
        matches!(err, BackupError::InMemory),
        "expected InMemory, got {:?}",
        err
    );
    // The message should say why rather than surfacing a raw SQLite error.
    assert!(err.to_string().contains("in memory"));
}

#[test]
fn backup_lands_beside_the_database_and_round_trips() {
    let tmp = TempDir::new("roundtrip");
    let db = Database::open(tmp.db_path()).unwrap();
    let conn = db.conn();
    add(&conn, "first");
    add(&conn, "second");

    let outcome = create_backup(&conn, "manual").unwrap();

    let backup_path = PathBuf::from(&outcome.path);
    assert!(backup_path.exists(), "backup file should exist");
    assert_eq!(
        backup_path.parent().unwrap(),
        tmp.0.join("backups"),
        "backups live in a `backups/` directory beside the database"
    );
    assert_eq!(outcome.total_backups, 1);
    assert_eq!(outcome.pruned, 0);

    assert_eq!(
        count_memories(&backup_path),
        2,
        "the backup must carry the rows, including anything still in the WAL"
    );
}

#[test]
fn a_backup_taken_before_a_write_does_not_contain_it() {
    let tmp = TempDir::new("snapshot");
    let db = Database::open(tmp.db_path()).unwrap();
    let conn = db.conn();
    add(&conn, "before");

    let outcome = create_backup(&conn, "manual").unwrap();
    add(&conn, "after");

    assert_eq!(
        count_memories(&PathBuf::from(&outcome.path)),
        1,
        "a backup is a point-in-time snapshot"
    );
    assert_eq!(count_memories(&tmp.db_path()), 2, "the live db moved on");
}

#[test]
fn successive_backups_do_not_collide_on_filename() {
    let tmp = TempDir::new("collide");
    let db = Database::open(tmp.db_path()).unwrap();
    let conn = db.conn();
    add(&conn, "content");

    // Microsecond precision is what keeps two backups in the same second apart.
    let first = create_backup(&conn, "manual").unwrap();
    let second = create_backup(&conn, "manual").unwrap();

    assert_ne!(first.path, second.path);
    assert_eq!(second.total_backups, 2);
}

#[test]
fn retention_prunes_the_oldest_backups() {
    let tmp = TempDir::new("retention");
    let db = Database::open(tmp.db_path()).unwrap();
    let conn = db.conn();
    add(&conn, "content");

    for _ in 0..BACKUP_RETENTION_COUNT {
        create_backup(&conn, "manual").unwrap();
    }
    let at_limit = list_backups(&backup_dir(&conn).unwrap()).unwrap();
    assert_eq!(at_limit.len(), BACKUP_RETENTION_COUNT);
    let oldest = at_limit.last().unwrap().filename.clone();

    let outcome = create_backup(&conn, "manual").unwrap();

    assert_eq!(
        outcome.total_backups, BACKUP_RETENTION_COUNT,
        "retention holds the count steady"
    );
    assert_eq!(outcome.pruned, 1);

    let remaining = list_backups(&backup_dir(&conn).unwrap()).unwrap();
    assert!(
        !remaining.iter().any(|b| b.filename == oldest),
        "the oldest backup should be the one pruned"
    );
}

#[test]
fn listing_a_missing_backup_directory_is_empty_not_an_error() {
    let tmp = TempDir::new("missing");
    let db = Database::open(tmp.db_path()).unwrap();

    let dir = backup_dir(&db.conn()).unwrap();
    assert!(!dir.exists());
    assert!(list_backups(&dir).unwrap().is_empty());
}

#[test]
fn a_label_cannot_escape_the_backup_directory() {
    let tmp = TempDir::new("traversal");
    let db = Database::open(tmp.db_path()).unwrap();
    let conn = db.conn();
    add(&conn, "content");

    // The tool never takes a caller-supplied label today, but the slugging is
    // what guarantees that stays true if one is ever plumbed through.
    let outcome = create_backup(&conn, "../../etc/passwd").unwrap();

    let path = PathBuf::from(&outcome.path);
    assert_eq!(
        path.parent().unwrap(),
        tmp.0.join("backups"),
        "a traversal label must not move the destination"
    );
    assert!(!outcome.path.contains(".."));
}

#[test]
fn an_empty_label_falls_back_rather_than_producing_a_bare_timestamp() {
    let tmp = TempDir::new("emptylabel");
    let db = Database::open(tmp.db_path()).unwrap();
    let conn = db.conn();
    add(&conn, "content");

    let outcome = create_backup(&conn, "---").unwrap();
    let filename = PathBuf::from(&outcome.path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(filename.starts_with("manual-"), "got {}", filename);
}
