//! The SQLite backend (ADR-0035, R3): every storage port over one
//! `rusqlite` connection, so the R3 benchmark can measure SQLite on the
//! same suites as the engine. Production still opens the engine.
//!
//! Each table group's adapter is a submodule holding its DDL (`SCHEMA`)
//! and its port impl. Tables are plain columns with the indexes their
//! reads filter on; a write that touches several rows runs in a savepoint
//! ([`SqliteTables::atomic`]), so it lands whole inside or outside a
//! [`super::backend::Backend`] transaction.

mod archives;
mod curation;
mod entities;
mod feedback;
mod history;
mod imports;
mod memories;
mod outbox;
mod promotions;
mod references;
mod related;
mod reminders;
mod saved_searches;
mod sessions;
mod stats;
mod testing;
mod vectors;
mod wiki;

use super::backend::{Backend, TransactionState};
use super::{Result, StoreError};
use rusqlite::{Connection, OptionalExtension, Params, Row};
use std::path::Path;

/// The DDL of every group, run on open. `IF NOT EXISTS` throughout, so a
/// reopen leaves existing tables alone.
const SCHEMAS: &[&str] = &[
    memories::SCHEMA,
    archives::SCHEMA,
    entities::SCHEMA,
    feedback::SCHEMA,
    history::SCHEMA,
    imports::SCHEMA,
    outbox::SCHEMA,
    promotions::SCHEMA,
    references::SCHEMA,
    related::SCHEMA,
    reminders::SCHEMA,
    saved_searches::SCHEMA,
    sessions::SCHEMA,
    stats::SCHEMA,
    vectors::SCHEMA,
    wiki::SCHEMA,
];

/// The database file inside a SQLite backend's directory.
const FILE: &str = "remind_me.sqlite3";

/// Every table group's rows in one SQLite database.
pub struct SqliteTables {
    conn: Connection,
    /// Set when a rollback failed: only reopening makes the store safe.
    failed: bool,
}

impl SqliteTables {
    /// Open (or create) the database in directory `dir`, in WAL mode with
    /// `synchronous = FULL`, so a commit that returned survives a crash.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if the directory or file cannot be opened or
    /// the schema created.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| StoreError::Engine(format!("sqlite: {}: {e}", dir.display())))?;
        let conn = Connection::open(dir.join(FILE)).map_err(sqlite_error)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")
            .map_err(sqlite_error)?;
        Self::with_schema(conn)
    }

    /// A private in-memory database, gone when dropped.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if the schema cannot be created.
    pub fn open_in_memory() -> Result<Self> {
        Self::with_schema(Connection::open_in_memory().map_err(sqlite_error)?)
    }

    fn with_schema(conn: Connection) -> Result<Self> {
        for schema in SCHEMAS {
            conn.execute_batch(schema).map_err(sqlite_error)?;
        }
        Ok(Self {
            conn,
            failed: false,
        })
    }

    /// Run `f` in a savepoint: its writes land together or not at all,
    /// nested inside an open transaction if there is one.
    pub(crate) fn atomic<T>(&mut self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let sp = self.conn.savepoint().map_err(sqlite_error)?;
        let value = f(&sp)?;
        sp.commit().map_err(sqlite_error)?;
        Ok(value)
    }
}

/// SQLite as a whole store (ADR-0035, step 6): a transaction is one
/// `BEGIN IMMEDIATE`, so a second writer waits and readers in other
/// connections see it whole or not at all.
impl Backend for SqliteTables {
    fn begin_transaction(&mut self) -> Result<()> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(sqlite_error)
    }

    /// A failed `COMMIT` leaves the transaction open (state 3) or, after an
    /// I/O error, rolled back by SQLite itself (state 1); either way
    /// [`Self::transaction_state`] reads which from the connection.
    fn commit_transaction(&mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT").map_err(sqlite_error)
    }

    fn transaction_state(&self) -> TransactionState {
        match (self.failed, self.conn.is_autocommit()) {
            (true, _) => TransactionState::NeedsRecovery,
            (false, true) => TransactionState::Closed,
            (false, false) => TransactionState::Open,
        }
    }

    fn rollback_transaction(&mut self) -> Result<()> {
        let rolled = self.conn.execute_batch("ROLLBACK").map_err(sqlite_error);
        if rolled.is_err() && !self.conn.is_autocommit() {
            self.failed = true;
        }
        rolled
    }

    fn rebuild_indexes(&mut self) -> Result<()> {
        self.atomic(memories::rebuild)?;
        self.rebuild_wiki_index()
    }

    /// Checkpoint the WAL, and `VACUUM` when a quarter or more of the file
    /// is free pages. `1` when it vacuumed. Skipped inside a transaction.
    fn compact(&mut self) -> Result<usize> {
        if !self.conn.is_autocommit() {
            return Ok(0);
        }
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(sqlite_error)?;
        let (free, total): (i64, i64) = self
            .one(
                "SELECT freelist_count, page_count FROM pragma_freelist_count(), pragma_page_count()",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?
            .unwrap_or((0, 0));
        if total == 0 || free * 4 < total {
            return Ok(0);
        }
        self.conn.execute_batch("VACUUM").map_err(sqlite_error)?;
        Ok(1)
    }

    /// A consistent copy by `VACUUM INTO`, as a directory [`Self::open`]
    /// opens.
    fn test_rows(&mut self) -> Option<&mut dyn crate::testing::TestRows> {
        Some(self)
    }

    fn backup_to(&self, dest: &Path) -> Result<()> {
        if dest.exists() {
            return Err(StoreError::Invalid(format!(
                "{} already exists",
                dest.display()
            )));
        }
        std::fs::create_dir_all(dest)
            .map_err(|e| StoreError::Engine(format!("sqlite: {}: {e}", dest.display())))?;
        let file = dest.join(FILE);
        self.conn
            .execute("VACUUM INTO ?1", [file.to_string_lossy()])
            .map(drop)
            .map_err(sqlite_error)
    }
}

/// Statement helpers shared by every adapter, so each port method is its
/// SQL and nothing else.
impl SqliteTables {
    /// Run one statement; how many rows it changed.
    pub(crate) fn exec(&self, sql: &str, params: impl Params) -> Result<usize> {
        exec(&self.conn, sql, params)
    }

    /// Every row `sql` selects, each read by `f`.
    pub(crate) fn all<T>(
        &self,
        sql: &str,
        params: impl Params,
        f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        all(&self.conn, sql, params, f)
    }

    /// The first row `sql` selects, if any, read by `f`.
    pub(crate) fn one<T>(
        &self,
        sql: &str,
        params: impl Params,
        f: impl FnOnce(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Option<T>> {
        self.conn
            .prepare_cached(sql)
            .and_then(|mut s| s.query_row(params, f).optional())
            .map_err(sqlite_error)
    }
}

/// [`SqliteTables::exec`] on any connection (or savepoint).
pub(crate) fn exec(c: &Connection, sql: &str, params: impl Params) -> Result<usize> {
    c.prepare_cached(sql)
        .and_then(|mut s| s.execute(params))
        .map_err(sqlite_error)
}

/// [`SqliteTables::all`] on any connection (or savepoint).
pub(crate) fn all<T>(
    c: &Connection,
    sql: &str,
    params: impl Params,
    f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = c.prepare_cached(sql).map_err(sqlite_error)?;
    let rows = stmt.query_map(params, f).map_err(sqlite_error)?;
    rows.collect::<rusqlite::Result<_>>().map_err(sqlite_error)
}

/// A SQLite failure as the store's error. `Engine` names the storage
/// backend that failed; adding a variant would change the public enum.
pub(crate) fn sqlite_error(e: rusqlite::Error) -> StoreError {
    StoreError::Engine(format!("sqlite: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, MemoryStore, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-11-21T00:00:00Z";

    /// A fresh directory under the system temp dir, removed first.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("remind_me_sqlite_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A database opened on SQLite runs its transactions and whole-store
    /// operations through it.
    #[test]
    fn a_database_opens_on_sqlite() {
        let db = Database::open_backend(Box::new(SqliteTables::open_in_memory().unwrap()), None)
            .unwrap();
        let store = db.store();
        let refused: Result<()> = store.transaction(|page| {
            Memories::new(page).insert(&NewMemory::new("m1", "quokka", NOW))?;
            Err(StoreError::Invalid("give up".into()))
        });
        assert!(refused.is_err());
        assert!(!Memories::new(&store).exists("m1").unwrap(), "rolled back");
        store
            .transaction(|page| Memories::new(page).insert(&NewMemory::new("m1", "quokka", NOW)))
            .unwrap();
        assert!(Memories::new(&store).exists("m1").unwrap(), "committed");
        crate::db::derived::rebuild_indexes(&store).unwrap();
        drop(store);
        db.compact_store().unwrap();
    }

    #[test]
    fn transaction_state_follows_the_connection() {
        let mut t = SqliteTables::open_in_memory().unwrap();
        assert_eq!(t.transaction_state(), TransactionState::Closed);
        t.begin_transaction().unwrap();
        assert_eq!(t.transaction_state(), TransactionState::Open);
        assert!(t.begin_transaction().is_err(), "transactions do not nest");
        t.rollback_transaction().unwrap();
        assert_eq!(t.transaction_state(), TransactionState::Closed);
    }

    /// A backup is a directory [`SqliteTables::open`] opens, holding what
    /// the store held; an existing destination is refused.
    #[test]
    fn a_backup_reopens_with_the_same_rows() {
        let dir = scratch("backup");
        let mut t = SqliteTables::open(&dir.join("live")).unwrap();
        MemoryStore::insert(&mut t, &NewMemory::new("m1", "quokka", NOW)).unwrap();
        let copy = dir.join("copy");
        t.backup_to(&copy).unwrap();
        assert!(t.backup_to(&copy).is_err(), "an existing destination");
        let restored = SqliteTables::open(&copy).unwrap();
        assert!(MemoryStore::exists(&restored, "m1").unwrap());
        drop((t, restored));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compact_vacuums_only_a_mostly_free_file() {
        let dir = scratch("compact");
        let mut t = SqliteTables::open(&dir).unwrap();
        assert_eq!(t.compact().unwrap(), 0, "nothing free");
        let body = "x".repeat(4096);
        for i in 0..200 {
            MemoryStore::insert(&mut t, &NewMemory::new(&format!("m{i}"), &body, NOW)).unwrap();
        }
        t.exec("DELETE FROM memories", []).unwrap();
        assert_eq!(t.compact().unwrap(), 1, "vacuumed");
        drop(t);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
