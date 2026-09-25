//! Coverage for the pre-migration snapshot guard (issue #95, matching
//! upstream's `_maybe_snapshot_before_migration`).
//!
//! The trigger logic lives as private helpers inside
//! `remind_me_core::db::migrations`, so these tests drive it the same way
//! `remind_me` does -- through `Database::open` -- and observe the
//! `backups/` directory it leaves (or doesn't leave) behind.

use remind_me_core::backup::list_backups;
use remind_me_core::db::queries;
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::path::PathBuf;

const SCHEMA_TABLES: &str = include_str!("../src/db/schema_tables.sql");

struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(
            format!(
                "rmm_migsnap_{}_{}_{:?}",
                tag,
                std::process::id(),
                std::thread::current().id()
            )
            .replace(['(', ')', ' '], ""),
        );
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.join("s.db"))
    }

    fn backups_dir(&self) -> PathBuf {
        self.0.parent().unwrap().join("backups")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(p) = self.0.parent() {
            let _ = std::fs::remove_dir_all(p);
        }
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

/// The old table shape this crate itself used to write, before it generated
/// the schema from `remind_me` -- `last_accessed_at` instead of
/// `accessed_at`. Standing in for "a database an older build of this crate,
/// or an older `remind_me`, produced".
const LEGACY_MEMORIES_TABLE: &str = "
    CREATE TABLE memories (
        id TEXT PRIMARY KEY, content TEXT NOT NULL,
        category TEXT NOT NULL DEFAULT 'general',
        tags TEXT NOT NULL DEFAULT '[]',
        source TEXT NOT NULL DEFAULT 'manual',
        metadata TEXT NOT NULL DEFAULT '{}',
        created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
        access_count INTEGER NOT NULL DEFAULT 0,
        last_accessed_at TEXT NOT NULL
    );
";

#[test]
fn opening_a_brand_new_database_takes_no_snapshot() {
    let tmp = TempDb::new("fresh");
    let _db = Database::open(&tmp.0).unwrap();
    assert!(
        !tmp.backups_dir().exists(),
        "a brand-new database has nothing yet worth protecting"
    );
}

#[test]
fn reopening_an_up_to_date_database_with_data_takes_no_snapshot() {
    let tmp = TempDb::new("current");
    {
        let db = Database::open(&tmp.0).unwrap();
        add(&db.conn(), "already on the current schema");
    }

    let _db = Database::open(&tmp.0).unwrap();

    assert!(
        !tmp.backups_dir().exists(),
        "reopening an already-reconciled database has no pending migration to guard"
    );
}

#[test]
fn a_legacy_database_with_data_is_snapshotted_before_reconciliation() {
    let tmp = TempDb::new("legacy-data");
    {
        let conn = Connection::open(&tmp.0).unwrap();
        conn.execute_batch(LEGACY_MEMORIES_TABLE).unwrap();
        conn.execute_batch(
            "
            INSERT INTO memories (id, content, created_at, updated_at, last_accessed_at)
            VALUES ('mem_old', 'protect me', '2020-06-15T12:00:00+00:00',
                    '2020-06-15T12:00:00+00:00', '2020-06-15T12:00:00+00:00');
            PRAGMA user_version = 19;
            ",
        )
        .unwrap();
    }

    let _db = Database::open(&tmp.0).unwrap();

    let backups = list_backups(&tmp.backups_dir()).unwrap();
    assert_eq!(
        backups.len(),
        1,
        "expected exactly one pre-migration snapshot"
    );
    assert!(
        backups[0].filename.contains("pre-migration-v19"),
        "got {}",
        backups[0].filename
    );
}

#[test]
fn a_legacy_database_with_no_rows_is_not_snapshotted() {
    let tmp = TempDb::new("legacy-empty");
    {
        let conn = Connection::open(&tmp.0).unwrap();
        conn.execute_batch(LEGACY_MEMORIES_TABLE).unwrap();
        conn.execute_batch("PRAGMA user_version = 19;").unwrap();
    }

    let _db = Database::open(&tmp.0).unwrap();

    assert!(
        !tmp.backups_dir().exists(),
        "an empty legacy table has nothing worth protecting, even though its shape needs reconciling"
    );
}

#[test]
fn an_old_version_stamp_is_snapshotted_under_its_own_label() {
    let tmp = TempDb::new("old-version");
    {
        let conn = Connection::open(&tmp.0).unwrap();
        conn.execute_batch(SCHEMA_TABLES).unwrap();
        conn.execute_batch(
            "
            INSERT INTO memories (id, content, created_at, updated_at)
            VALUES ('mem_old', 'protect me', '2020-06-15T12:00:00+00:00', '2020-06-15T12:00:00+00:00');
            PRAGMA user_version = 13;
            ",
        )
        .unwrap();
    }

    let _db = Database::open(&tmp.0).unwrap();

    let backups = list_backups(&tmp.backups_dir()).unwrap();
    assert_eq!(backups.len(), 1);
    assert!(
        backups[0].filename.contains("pre-migration-v13"),
        "the label should reflect the version read before migration, got {}",
        backups[0].filename
    );
}

#[test]
fn a_snapshot_failure_does_not_block_the_migration() {
    let tmp = TempDb::new("blocked-backups-dir");
    {
        let conn = Connection::open(&tmp.0).unwrap();
        conn.execute_batch(LEGACY_MEMORIES_TABLE).unwrap();
        conn.execute_batch(
            "
            INSERT INTO memories (id, content, created_at, updated_at, last_accessed_at)
            VALUES ('mem_old', 'protect me', '2020-06-15T12:00:00+00:00',
                    '2020-06-15T12:00:00+00:00', '2020-06-15T12:00:00+00:00');
            PRAGMA user_version = 19;
            ",
        )
        .unwrap();
    }

    // Put a plain file where the backups directory would go, so
    // `create_backup`'s `create_dir_all` fails -- standing in for the
    // reference's "disk full" case without needing real disk pressure.
    std::fs::write(tmp.backups_dir(), b"not a directory").unwrap();

    let result = Database::open(&tmp.0);

    result.expect("a failed snapshot must never block the migration it exists to protect");
}
