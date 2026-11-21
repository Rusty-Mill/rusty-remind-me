//! The SQLite backend (ADR-0035, R3): every storage port over one
//! `rusqlite` connection, so the R3 benchmark can measure SQLite on the
//! same suites as the engine. Production still opens the engine.
//!
//! Each table group's adapter is a submodule holding its DDL (`SCHEMA`)
//! and its port impl. Tables are plain columns with the indexes their
//! reads filter on; a write that touches several rows runs in a savepoint
//! ([`SqliteTables::atomic`]), so it lands whole inside or outside a
//! [`super::backend::Backend`] transaction.

mod feedback;
mod history;
mod memories;
mod reminders;
mod saved_searches;

use super::{Result, StoreError};
use rusqlite::{Connection, OptionalExtension, Params, Row};
use std::path::Path;

/// The DDL of every group, run on open. `IF NOT EXISTS` throughout, so a
/// reopen leaves existing tables alone.
const SCHEMAS: &[&str] = &[
    memories::SCHEMA,
    feedback::SCHEMA,
    history::SCHEMA,
    reminders::SCHEMA,
    saved_searches::SCHEMA,
];

/// Every table group's rows in one SQLite database.
pub struct SqliteTables {
    conn: Connection,
}

impl SqliteTables {
    /// Open (or create) the database file at `path`, in WAL mode with
    /// `synchronous = FULL`, so a commit that returned survives a crash.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if the file cannot be opened or its schema
    /// created.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(sqlite_error)?;
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
        Ok(Self { conn })
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
