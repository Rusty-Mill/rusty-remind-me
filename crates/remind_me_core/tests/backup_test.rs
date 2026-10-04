//! Coverage for `remind_me_backup`: a backup is a copy of the engine
//! directory, taken while the tables are held, beside the database file.

use remind_me_core::backup::{
    backup_dir, create_backup, list_backups, BackupError, BACKUP_RETENTION_COUNT,
};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::{Database, MemoryAddInput};
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

fn add(store: &Store<'_>, content: &str) {
    queries::add_memory(
        store,
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
            ..Default::default()
        },
    )
    .unwrap();
}

/// The memories in the engine directory `backup`, by restoring it where a
/// node looks for its store and opening that.
fn count_memories(backup: &Path) -> i64 {
    let restored = TempDir::new("restored");
    let engine = restored.0.join("remind_me.engine");
    std::fs::create_dir(&engine).unwrap();
    for entry in std::fs::read_dir(backup).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), engine.join(entry.file_name())).unwrap();
    }
    let db = Database::open(restored.db_path()).unwrap();
    let store = db.store();
    remind_me_core::testing::count(&store, remind_me_core::testing::Table::Memories).unwrap()
}

#[test]
fn backup_of_an_in_memory_database_is_refused_clearly() {
    let db = Database::open_in_memory().unwrap();
    let err = create_backup(&db.store(), "manual").unwrap_err();

    assert!(
        matches!(err, BackupError::InMemory),
        "expected InMemory, got {:?}",
        err
    );
    // The message should say why rather than surfacing a raw store error.
    assert!(err.to_string().contains("in memory"));
}

#[test]
fn backup_lands_beside_the_database_and_round_trips() {
    let tmp = TempDir::new("roundtrip");
    let db = Database::open(tmp.db_path()).unwrap();
    let store = db.store();
    add(&store, "first");
    add(&store, "second");

    let outcome = create_backup(&store, "manual").unwrap();

    let backup_path = PathBuf::from(&outcome.path);
    assert!(backup_path.is_dir(), "a backup is an engine directory");
    assert!(outcome.path.ends_with(".engine"), "{}", outcome.path);
    assert_eq!(
        backup_path.parent().unwrap(),
        tmp.0.join("backups"),
        "backups live in a `backups/` directory beside the database"
    );
    assert!(
        !backup_path.join("node.lock").exists(),
        "the directory lock is the live store's, not the backup's"
    );
    assert_eq!(outcome.total_backups, 1);
    assert_eq!(outcome.pruned, 0);
    let listed = list_backups(&backup_dir(&store).unwrap()).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].size_bytes > 0);

    assert_eq!(
        count_memories(&backup_path),
        2,
        "the backup must carry the rows, including anything still in the journal"
    );
}

#[test]
fn a_backup_taken_before_a_write_does_not_contain_it() {
    let tmp = TempDir::new("snapshot");
    let db = Database::open(tmp.db_path()).unwrap();
    let store = db.store();
    add(&store, "before");

    let outcome = create_backup(&store, "manual").unwrap();
    add(&store, "after");

    assert_eq!(
        count_memories(&PathBuf::from(&outcome.path)),
        1,
        "a backup is a point-in-time snapshot"
    );
    assert_eq!(
        remind_me_core::testing::count(&store, remind_me_core::testing::Table::Memories).unwrap(),
        2,
        "the live store moved on"
    );
}

#[test]
fn successive_backups_do_not_collide_on_filename() {
    let tmp = TempDir::new("collide");
    let db = Database::open(tmp.db_path()).unwrap();
    let store = db.store();
    add(&store, "content");

    // Microsecond precision is what keeps two backups in the same second apart.
    let first = create_backup(&store, "manual").unwrap();
    let second = create_backup(&store, "manual").unwrap();

    assert_ne!(first.path, second.path);
    assert_eq!(second.total_backups, 2);
}

#[test]
fn retention_prunes_the_oldest_backups() {
    let tmp = TempDir::new("retention");
    let db = Database::open(tmp.db_path()).unwrap();
    let store = db.store();
    add(&store, "content");

    for _ in 0..BACKUP_RETENTION_COUNT {
        create_backup(&store, "manual").unwrap();
    }
    let at_limit = list_backups(&backup_dir(&store).unwrap()).unwrap();
    assert_eq!(at_limit.len(), BACKUP_RETENTION_COUNT);
    let oldest = at_limit.last().unwrap().clone();

    let outcome = create_backup(&store, "manual").unwrap();

    assert_eq!(
        outcome.total_backups, BACKUP_RETENTION_COUNT,
        "retention holds the count steady"
    );
    assert_eq!(outcome.pruned, 1);

    let remaining = list_backups(&backup_dir(&store).unwrap()).unwrap();
    assert!(
        !remaining.iter().any(|b| b.filename == oldest.filename),
        "the oldest backup should be the one pruned"
    );
    assert!(!Path::new(&oldest.path).exists(), "{} should be gone", oldest.path);
}

#[test]
fn backups_from_the_sqlite_days_still_list_and_are_pruned_in_turn() {
    let tmp = TempDir::new("legacy");
    let db = Database::open(tmp.db_path()).unwrap();
    let store = db.store();
    add(&store, "content");
    let dir = backup_dir(&store).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    // A `.db` file a build before ADR-0025 wrote: listed by its extension,
    // oldest by its mtime once the engine backups below arrive.
    let old = dir.join("manual-20200101T000000000000Z.db");
    std::fs::write(&old, b"an old SQLite backup").unwrap();
    assert_eq!(list_backups(&dir).unwrap().len(), 1);

    for _ in 0..BACKUP_RETENTION_COUNT {
        create_backup(&store, "manual").unwrap();
    }
    assert!(!old.exists(), "the SQLite backup was the oldest, so it went");
    assert_eq!(list_backups(&dir).unwrap().len(), BACKUP_RETENTION_COUNT);
}

#[test]
fn listing_a_missing_backup_directory_is_empty_not_an_error() {
    let tmp = TempDir::new("missing");
    let db = Database::open(tmp.db_path()).unwrap();

    let dir = backup_dir(&db.store()).unwrap();
    assert!(!dir.exists());
    assert!(list_backups(&dir).unwrap().is_empty());
}

#[test]
fn a_label_cannot_escape_the_backup_directory() {
    let tmp = TempDir::new("traversal");
    let db = Database::open(tmp.db_path()).unwrap();
    let store = db.store();
    add(&store, "content");

    // The tool never takes a caller-supplied label today, but the slugging is
    // what guarantees that stays true if one is ever plumbed through.
    let outcome = create_backup(&store, "../../etc/passwd").unwrap();

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
    let store = db.store();
    add(&store, "content");

    let outcome = create_backup(&store, "---").unwrap();
    let filename = PathBuf::from(&outcome.path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(filename.starts_with("manual-"), "got {}", filename);
}

#[test]
fn a_partial_copy_left_by_a_crash_does_not_list_as_a_backup() {
    let tmp = TempDir::new("partial");
    let db = Database::open(tmp.db_path()).unwrap();
    let store = db.store();
    add(&store, "content");
    let dir = backup_dir(&store).unwrap();
    std::fs::create_dir_all(dir.join("manual-20200101T000000000000Z.partial")).unwrap();
    assert!(list_backups(&dir).unwrap().is_empty());
    let outcome = create_backup(&store, "manual").unwrap();
    assert_eq!(outcome.total_backups, 1);
}
