//! The dialog store on its own SQLite file (D76): [`SqliteDialogs`].
//!
//! Append-only is enforced in the file itself: triggers abort any UPDATE or
//! DELETE, so a bug elsewhere in the process cannot rewrite a dialog.

use super::sqlite_error;
use crate::db::dialogs::{Appended, DialogRecord, DialogStore};
use crate::db::{Result, StoreError};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;

const FILE: &str = "dialogs.sqlite3";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS dialogs (
    id TEXT PRIMARY KEY, capture_id TEXT NOT NULL, session_id TEXT,
    project TEXT, created_at TEXT NOT NULL, head TEXT NOT NULL,
    body_hash TEXT NOT NULL, body_bytes INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS dialogs_capture ON dialogs (capture_id, created_at, id);
CREATE TRIGGER IF NOT EXISTS dialogs_no_update BEFORE UPDATE ON dialogs
    BEGIN SELECT RAISE(ABORT, 'dialogs are append-only'); END;
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

    fn with_schema(conn: Connection) -> Result<Self> {
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
        if let Some(existing) = self.get(&r.id)? {
            return if existing == *r {
                Ok(Appended::Duplicate)
            } else {
                Err(StoreError::Invalid(format!(
                    "dialog {} already exists with different content",
                    r.id
                )))
            };
        }
        let bytes = i64::try_from(r.body_bytes)
            .map_err(|_| StoreError::Invalid("dialog body is too large".into()))?;
        self.conn
            .execute(
                &format!("INSERT INTO dialogs ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"),
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
        Ok(Appended::Inserted)
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
    fn a_missing_or_altered_blob_is_an_error_not_silence() {
        let dir = scratch("tamper");
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
