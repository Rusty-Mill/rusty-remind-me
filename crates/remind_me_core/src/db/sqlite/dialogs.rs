//! The dialog store on its own SQLite file (D76): [`SqliteDialogs`].
//!
//! Append-only is enforced in the file itself: triggers abort any UPDATE or
//! DELETE, so a bug elsewhere in the process cannot rewrite a dialog.

use super::sqlite_error;
use crate::db::dialogs::{Appended, DialogRecord, DialogStore};
use crate::db::{Result, StoreError};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};
use std::path::Path;

/// The dialog file inside a store (or backup package) directory.
pub const FILE: &str = "dialogs.sqlite3";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS dialogs (
    id TEXT NOT NULL PRIMARY KEY, capture_id TEXT NOT NULL, session_id TEXT,
    project TEXT, created_at TEXT NOT NULL, head TEXT NOT NULL,
    body_hash TEXT NOT NULL, body_bytes INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS dialogs_capture ON dialogs (capture_id, created_at, id);
CREATE TRIGGER IF NOT EXISTS dialogs_no_update BEFORE UPDATE ON dialogs
    BEGIN SELECT RAISE(ABORT, 'dialogs are append-only'); END;
CREATE TRIGGER IF NOT EXISTS dialogs_no_replace BEFORE INSERT ON dialogs
    WHEN EXISTS (SELECT 1 FROM dialogs WHERE id = NEW.id)
    BEGIN SELECT RAISE(IGNORE); END;
CREATE TRIGGER IF NOT EXISTS dialogs_no_delete BEFORE DELETE ON dialogs
    BEGIN SELECT RAISE(ABORT, 'dialogs are append-only'); END;
";

const COLUMNS: &str =
    "id, capture_id, session_id, project, created_at, head, body_hash, body_bytes";

/// Dialogs in `<dir>/dialogs.sqlite3`, WAL with `synchronous = FULL`.
pub struct SqliteDialogs {
    conn: Connection,
}

impl SqliteDialogs {
    /// Open (or create) the dialog file in `dir`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if the directory or file cannot be opened.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| StoreError::Engine(format!("dialogs: {}: {e}", dir.display())))?;
        let conn = Connection::open(dir.join(FILE)).map_err(sqlite_error)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")
            .map_err(sqlite_error)?;
        Self::with_schema(conn)
    }

    /// A private in-memory store, gone when dropped.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if the schema cannot be created.
    pub fn open_in_memory() -> Result<Self> {
        Self::with_schema(Connection::open_in_memory().map_err(sqlite_error)?)
    }

    /// Open an existing dialog file read-only: a backup package or a
    /// restored copy being checked. Nothing is created or changed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if the file is missing or not SQLite.
    pub fn open_read_only(file: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sqlite_error)?;
        Ok(Self { conn })
    }

    /// Write a consistent copy of the store to `file` (`VACUUM INTO`, D78),
    /// then sync it, since SQLite does not sync that output. The copy keeps
    /// the triggers, so it is append-only too. `file` must not exist.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a path that is not UTF-8, otherwise
    /// [`StoreError::Engine`].
    pub fn snapshot_into(&self, file: &Path) -> Result<()> {
        let target = file
            .to_str()
            .ok_or_else(|| StoreError::Invalid(format!("{}: not UTF-8", file.display())))?;
        self.conn
            .execute("VACUUM INTO ?1", [target])
            .map_err(sqlite_error)?;
        std::fs::File::open(file)
            .and_then(|f| f.sync_all())
            .map_err(|e| StoreError::Engine(format!("dialogs: {}: {e}", file.display())))
    }

    fn with_schema(conn: Connection) -> Result<Self> {
        // The file refuses REPLACE by itself (`dialogs_no_replace`, which
        // does not depend on any connection setting). This pragma is a
        // second line: with it on, REPLACE's implicit delete also fires
        // the delete trigger below.
        conn.execute_batch("PRAGMA recursive_triggers = ON;")
            .map_err(sqlite_error)?;
        conn.execute_batch(SCHEMA).map_err(sqlite_error)?;
        Ok(Self { conn })
    }
}

fn record(row: &Row<'_>) -> rusqlite::Result<DialogRecord> {
    Ok(DialogRecord {
        id: row.get(0)?,
        capture_id: row.get(1)?,
        session_id: row.get(2)?,
        project: row.get(3)?,
        created_at: row.get(4)?,
        head: row.get(5)?,
        body_hash: row.get(6)?,
        body_bytes: row.get::<_, i64>(7)?.try_into().unwrap_or(0),
    })
}

impl DialogStore for SqliteDialogs {
    fn append(&mut self, r: &DialogRecord) -> Result<Appended> {
        let bytes = i64::try_from(r.body_bytes)
            .map_err(|_| StoreError::Invalid("dialog body is too large".into()))?;
        // One statement decides insert-or-not, so two connections racing on
        // an id cannot both miss it; the loser reads the winner's row.
        let inserted = self
            .conn
            .execute(
                &format!(
                    "INSERT INTO dialogs ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                     ON CONFLICT(id) DO NOTHING"
                ),
                params![
                    r.id,
                    r.capture_id,
                    r.session_id,
                    r.project,
                    r.created_at,
                    r.head,
                    r.body_hash,
                    bytes
                ],
            )
            .map_err(sqlite_error)?;
        if inserted == 1 {
            return Ok(Appended::Inserted);
        }
        match self.get(&r.id)? {
            Some(existing) if existing == *r => Ok(Appended::Duplicate),
            Some(_) => Err(StoreError::Invalid(format!(
                "dialog {} already exists with different content",
                r.id
            ))),
            None => Err(StoreError::Engine(format!(
                "dialog {} conflicted but is not readable",
                r.id
            ))),
        }
    }

    fn get(&self, id: &str) -> Result<Option<DialogRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM dialogs WHERE id = ?1"),
                [id],
                record,
            )
            .optional()
            .map_err(sqlite_error)
    }

    fn for_capture(&self, capture_id: &str) -> Result<Vec<DialogRecord>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM dialogs WHERE capture_id = ?1 ORDER BY created_at, id"
            ))
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([capture_id], record)
            .map_err(sqlite_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sqlite_error)?;
        Ok(rows)
    }

    fn count(&self) -> Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM dialogs", [], |r| r.get::<_, i64>(0))
            .map(|n| n.try_into().unwrap_or(0))
            .map_err(sqlite_error)
    }

    fn scan(&self, visit: &mut dyn FnMut(DialogRecord) -> Result<()>) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM dialogs ORDER BY created_at, id"
            ))
            .map_err(sqlite_error)?;
        for row in stmt.query_map([], record).map_err(sqlite_error)? {
            visit(row.map_err(sqlite_error)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blobs::BlobStore;
    use crate::db::dialogs::{DialogLog, NewDialog};
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dialogs_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    crate::dialog_conformance!(SqliteDialogs::open_in_memory().unwrap());

    #[test]
    fn update_and_delete_are_refused_by_the_file_itself() {
        let mut s = SqliteDialogs::open_in_memory().unwrap();
        s.append(&crate::db::dialogs::conformance::record(
            "d1",
            "c1",
            "2026-11-25T00:00:00Z",
            "hi",
        ))
        .unwrap();
        let upd = s.conn.execute("UPDATE dialogs SET head = 'x'", []);
        let del = s.conn.execute("DELETE FROM dialogs", []);
        assert!(upd.unwrap_err().to_string().contains("append-only"));
        assert!(del.unwrap_err().to_string().contains("append-only"));
        assert_eq!(s.get("d1").unwrap().unwrap().head, "hi");
    }

    #[test]
    fn a_snapshot_holds_every_row_and_stays_append_only() {
        let dir = scratch("snapshot");
        let mut s = SqliteDialogs::open(&dir).unwrap();
        let r = crate::db::dialogs::conformance::record("d1", "c1", "2026-11-25T00:00:00Z", "hi");
        s.append(&r).unwrap();
        let copy = dir.join("copy.sqlite3");
        s.snapshot_into(&copy).unwrap();
        let snap = SqliteDialogs::open_read_only(&copy).unwrap();
        assert_eq!(snap.get("d1").unwrap(), Some(r));
        // The triggers came along: a writable handle still cannot delete.
        let rw = Connection::open(&copy).unwrap();
        assert!(rw.execute("DELETE FROM dialogs", []).is_err());
        // A second snapshot onto an existing file is refused.
        assert!(s.snapshot_into(&copy).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_only_handle_refuses_appends_and_missing_files() {
        let dir = scratch("readonly");
        SqliteDialogs::open(&dir).unwrap();
        let mut ro = SqliteDialogs::open_read_only(&dir.join(FILE)).unwrap();
        let r = crate::db::dialogs::conformance::record("d1", "c1", "2026-11-25T00:00:00Z", "hi");
        assert!(ro.append(&r).is_err());
        assert!(SqliteDialogs::open_read_only(&dir.join("absent.sqlite3")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_null_id_is_refused() {
        let s = SqliteDialogs::open_in_memory().unwrap();
        let null_id =
            "INSERT INTO dialogs (id, capture_id, created_at, head, body_hash, body_bytes) \
                       VALUES (NULL, 'c', 't', 'h', 'x', 1)";
        assert!(s.conn.execute(null_id, []).is_err());
        assert!(s.conn.execute(null_id, []).is_err());
        assert_eq!(s.count().unwrap(), 0);
    }

    #[test]
    fn update_delete_and_replace_leave_a_row_unchanged() {
        let mut s = SqliteDialogs::open_in_memory().unwrap();
        let r = crate::db::dialogs::conformance::record("d1", "c1", "2026-11-25T00:00:00Z", "hi");
        s.append(&r).unwrap();
        for sql in [
            "UPDATE dialogs SET head = 'x' WHERE id = 'd1'",
            "DELETE FROM dialogs WHERE id = 'd1'",
            "INSERT OR REPLACE INTO dialogs (id, capture_id, created_at, head, body_hash, body_bytes) \
             VALUES ('d1', 'other', 't', 'changed', 'y', 9)",
            "REPLACE INTO dialogs (id, capture_id, created_at, head, body_hash, body_bytes) \
             VALUES ('d1', 'other', 't', 'changed', 'y', 9)",
        ] {
            // UPDATE and DELETE abort; REPLACE is ignored (zero rows).
            if let Ok(changed) = s.conn.execute(sql, []) {
                assert_eq!(changed, 0, "{sql} wrote a row");
            }
            assert_eq!(s.get("d1").unwrap(), Some(r.clone()), "{sql} changed the row");
        }
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn a_plain_second_connection_cannot_replace_a_dialog() {
        let dir = scratch("plain");
        let mut s = SqliteDialogs::open(&dir).unwrap();
        let r = crate::db::dialogs::conformance::record("d1", "c1", "2026-11-25T00:00:00Z", "hi");
        s.append(&r).unwrap();
        // A default-settings connection: recursive triggers off, no pragmas.
        let plain = Connection::open(dir.join(FILE)).unwrap();
        for sql in [
            "INSERT OR REPLACE INTO dialogs (id, capture_id, created_at, head, body_hash, body_bytes) \
             VALUES ('d1', 'other', 't', 'changed', 'y', 9)",
            "REPLACE INTO dialogs (id, capture_id, created_at, head, body_hash, body_bytes) \
             VALUES ('d1', 'other', 't', 'changed', 'y', 9)",
        ] {
            // The trigger ignores the insert (zero rows) rather than erroring;
            // either way the stored row must be untouched.
            if let Ok(changed) = plain.execute(sql, []) {
                assert_eq!(changed, 0, "{sql} wrote a row");
            }
            assert_eq!(s.get("d1").unwrap(), Some(r.clone()), "{sql} changed the row");
        }
        assert_eq!(s.count().unwrap(), 1);
        drop(plain);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_log_stores_the_body_once_and_reads_it_back_verified() {
        let dir = scratch("log");
        let store = SqliteDialogs::open(&dir.join("db")).unwrap();
        let mut log = DialogLog::new(store, BlobStore::new(dir.join("blobs")));
        let body = "x".repeat(2000);
        let d = NewDialog {
            id: "d1",
            capture_id: "c1",
            session_id: None,
            project: Some("p"),
            created_at: "2026-11-25T00:00:00Z",
            body: &body,
        };
        assert_eq!(log.record(&d).unwrap(), Appended::Inserted);
        assert_eq!(log.record(&d).unwrap(), Appended::Duplicate);
        let rec = log.store().get("d1").unwrap().unwrap();
        assert_eq!(rec.head.len(), 512);
        assert_eq!(rec.body_bytes, 2000);
        assert_eq!(log.body(&rec).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_connections_racing_on_one_id_classify_correctly() {
        let dir = scratch("race");
        let mut a = SqliteDialogs::open(&dir).unwrap();
        let mut b = SqliteDialogs::open(&dir).unwrap();
        let first =
            crate::db::dialogs::conformance::record("d1", "c1", "2026-11-25T00:00:00Z", "one");
        let other =
            crate::db::dialogs::conformance::record("d1", "c1", "2026-11-25T00:00:00Z", "two");
        assert_eq!(a.append(&first).unwrap(), Appended::Inserted);
        // `b` never saw the row before its own append: identical is a
        // duplicate, different is refused, and the original is untouched.
        assert_eq!(b.append(&first).unwrap(), Appended::Duplicate);
        assert!(matches!(b.append(&other), Err(StoreError::Invalid(_))));
        assert_eq!(a.get("d1").unwrap(), Some(first));
        assert_eq!(a.count().unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_pre_existing_blob_is_replaced_before_the_row_commits() {
        let dir = scratch("corrupt");
        let blobs = BlobStore::new(dir.join("blobs"));
        let key = crate::blobs::content_key(b"hello");
        std::fs::create_dir_all(blobs.path(&key).parent().unwrap()).unwrap();
        std::fs::write(blobs.path(&key), b"torn").unwrap();
        let mut log = DialogLog::new(SqliteDialogs::open_in_memory().unwrap(), blobs.clone());
        let d = NewDialog {
            id: "d1",
            capture_id: "c1",
            session_id: None,
            project: None,
            created_at: "2026-11-25T00:00:00Z",
            body: "hello",
        };
        assert_eq!(log.record(&d).unwrap(), Appended::Inserted);
        let rec = log.store().get("d1").unwrap().unwrap();
        assert_eq!(log.body(&rec).unwrap(), "hello");
        assert!(blobs.verify(&key).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_first_write_creates_the_blob_root_and_shard_under_an_existing_anchor() {
        let dir = scratch("fresh");
        std::fs::create_dir_all(&dir).unwrap();
        // The anchor (`dir`) exists; the blob root and shard do not yet.
        let root = dir.join("blobs");
        let mut log = DialogLog::new(
            SqliteDialogs::open_in_memory().unwrap(),
            BlobStore::new(&root),
        );
        let d = NewDialog {
            id: "d1",
            capture_id: "c1",
            session_id: None,
            project: None,
            created_at: "2026-11-25T00:00:00Z",
            body: "hello",
        };
        assert_eq!(log.record(&d).unwrap(), Appended::Inserted);
        let rec = log.store().get("d1").unwrap().unwrap();
        assert_eq!(log.body(&rec).unwrap(), "hello");
        // A second write into the now-existing hierarchy still succeeds.
        let again = NewDialog { id: "d2", ..d };
        assert_eq!(log.record(&again).unwrap(), Appended::Inserted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_anchor_is_refused_and_appends_no_row() {
        let dir = scratch("noanchor");
        // The blob root's parent does not exist, so nothing durable can
        // be promised under it: refused, not silently created.
        let root = dir.join("deep").join("blobs");
        let mut log = DialogLog::new(
            SqliteDialogs::open_in_memory().unwrap(),
            BlobStore::new(&root),
        );
        let d = NewDialog {
            id: "d1",
            capture_id: "c1",
            session_id: None,
            project: None,
            created_at: "2026-11-25T00:00:00Z",
            body: "hello",
        };
        assert!(log.record(&d).is_err());
        assert_eq!(log.store().count().unwrap(), 0);
        assert!(!dir.exists());
    }

    #[test]
    fn a_retry_with_the_directories_already_present_still_succeeds() {
        let dir = scratch("retry");
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("blobs");
        let mut log = DialogLog::new(
            SqliteDialogs::open_in_memory().unwrap(),
            BlobStore::new(&root),
        );
        let d = NewDialog {
            id: "d1",
            capture_id: "c1",
            session_id: None,
            project: None,
            created_at: "2026-11-25T00:00:00Z",
            body: "hello",
        };
        // A first attempt left the root and shard behind but no row.
        let shard = BlobStore::new(&root).path(&crate::blobs::content_key(b"hello"));
        std::fs::create_dir_all(shard.parent().unwrap()).unwrap();
        assert_eq!(log.record(&d).unwrap(), Appended::Inserted);
        // The identical record again takes the verified-existing path.
        assert_eq!(log.record(&d).unwrap(), Appended::Duplicate);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_publication_appends_no_row() {
        let dir = scratch("nopublish");
        std::fs::create_dir_all(&dir).unwrap();
        // The blob root is a file, so no blob can be created under it.
        let root = dir.join("blobs");
        std::fs::write(&root, b"not a directory").unwrap();
        let mut log = DialogLog::new(
            SqliteDialogs::open_in_memory().unwrap(),
            BlobStore::new(&root),
        );
        let d = NewDialog {
            id: "d1",
            capture_id: "c1",
            session_id: None,
            project: None,
            created_at: "2026-11-25T00:00:00Z",
            body: "hello",
        };
        assert!(log.record(&d).is_err());
        assert_eq!(log.store().count().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_altered_blob_is_an_error_not_silence() {
        let dir = scratch("tamper");
        std::fs::create_dir_all(&dir).unwrap();
        let blobs = BlobStore::new(dir.join("blobs"));
        let mut log = DialogLog::new(SqliteDialogs::open_in_memory().unwrap(), blobs.clone());
        let d = NewDialog {
            id: "d1",
            capture_id: "c1",
            session_id: None,
            project: None,
            created_at: "2026-11-25T00:00:00Z",
            body: "hello",
        };
        log.record(&d).unwrap();
        let rec = log.store().get("d1").unwrap().unwrap();
        std::fs::write(blobs.path(&rec.body_hash), b"tampered").unwrap();
        assert!(log.body(&rec).is_err());
        std::fs::remove_file(blobs.path(&rec.body_hash)).unwrap();
        assert!(log.body(&rec).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
