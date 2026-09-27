//! The store handle every caller takes, and the error every repository
//! returns (ADR-0023, phase 4).
//!
//! Callers used to take a `rusqlite::Connection`, which tied every one of
//! them to SQLite. They take a [`Store`] instead, and the repositories behind
//! it decide how to answer. Today there is one backend, SQLite; the engine
//! joins it as a second variant, group by group, and SQLite leaves in phase
//! 6. The backends are a closed, temporary pair, so the seam is an enum each
//! repository matches on, not a trait.
//!
//! A [`Store`] holds the database's lock for as long as it lives, as the
//! connection guard it replaces did.

use parking_lot::MutexGuard;
use rusqlite::Connection;
use std::fmt;

/// A handle on the node's store, holding its lock.
pub struct Store<'a> {
    backend: Backend<'a>,
}

enum Backend<'a> {
    /// The process-wide connection, locked.
    SqliteLocked(MutexGuard<'a, Connection>),
    /// A connection owned elsewhere: a background worker's own, or one a
    /// test or tool opened directly.
    SqliteBorrowed(&'a Connection),
}

impl<'a> Store<'a> {
    pub(crate) fn locked(guard: MutexGuard<'a, Connection>) -> Self {
        Self {
            backend: Backend::SqliteLocked(guard),
        }
    }

    /// A store over a SQLite connection opened elsewhere.
    pub fn over_sqlite(conn: &'a Connection) -> Self {
        Self {
            backend: Backend::SqliteBorrowed(conn),
        }
    }

    /// The SQLite connection underneath, for code that must speak SQL: the
    /// schema and its migrations, and tests that inspect rows directly.
    /// `None` once a store is backed by something else.
    pub fn sqlite(&self) -> Option<&Connection> {
        Some(self.conn())
    }

    /// The connection, for the SQLite repositories.
    pub(crate) fn conn(&self) -> &Connection {
        match &self.backend {
            Backend::SqliteLocked(guard) => guard,
            Backend::SqliteBorrowed(conn) => conn,
        }
    }
}

/// Why a store operation failed, whatever the backend.
#[derive(Debug)]
pub enum StoreError {
    /// The row asked for does not exist.
    NotFound,
    /// The request itself was not valid: a bad argument, not a failure of
    /// the store.
    Invalid(String),
    /// SQLite failed.
    Sqlite(rusqlite::Error),
}

/// What every repository returns.
pub type Result<T> = std::result::Result<T, StoreError>;

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The message SQLite gave for this, which callers and tests
            // have long matched on.
            StoreError::NotFound => write!(f, "Query returned no rows"),
            StoreError::Invalid(why) => write!(f, "{why}"),
            StoreError::Sqlite(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound,
            e => StoreError::Sqlite(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_rows_becomes_not_found_and_keeps_its_message() {
        let e: StoreError = rusqlite::Error::QueryReturnedNoRows.into();
        assert!(matches!(e, StoreError::NotFound));
        assert_eq!(
            e.to_string(),
            rusqlite::Error::QueryReturnedNoRows.to_string()
        );
    }

    #[test]
    fn a_borrowed_connection_is_reachable() {
        let conn = Connection::open_in_memory().unwrap();
        let store = Store::over_sqlite(&conn);
        let one: i64 = store
            .sqlite()
            .unwrap()
            .query_row("SELECT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(one, 1);
    }
}
