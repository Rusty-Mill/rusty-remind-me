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
//!
//! With the `engine-store` feature, a store can also carry the engine tables
//! (`db::engine`). The groups moved so far read and write there; every
//! other group stays on the SQLite connection until its own step.

#[cfg(feature = "engine-store")]
use super::engine::{EngineHandle, EngineLock};
use parking_lot::MutexGuard;
use rusqlite::Connection;
use std::fmt;
use std::path::{Path, PathBuf};

/// A handle on the node's store, holding its lock.
pub struct Store<'a> {
    backend: Backend<'a>,
    /// The engine tables, when the database was opened on the engine. Locked
    /// per call: a repository never nests one call inside another, and the
    /// SQLite lock this store holds already orders the callers.
    #[cfg(feature = "engine-store")]
    engine: Option<EngineHandle>,
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
            #[cfg(feature = "engine-store")]
            engine: None,
        }
    }

    /// This store, with the engine tables beside its connection.
    #[cfg(feature = "engine-store")]
    pub(crate) fn with_engine(self, engine: Option<EngineHandle>) -> Self {
        Self { engine, ..self }
    }

    /// Run `work` as one transaction: every write it makes lands, or none
    /// does. On SQLite that is a transaction on this store's connection;
    /// when the tables hold the memories core, the core's writes are one
    /// page there (`db::engine::page`), finished before SQLite commits.
    ///
    /// `work` gets a store to write through. An error from it, or a panic,
    /// rolls both back.
    ///
    /// # Errors
    ///
    /// Whatever `work` returns, or [`StoreError`] if the transaction cannot
    /// begin or commit.
    pub fn transaction<T, E>(
        &self,
        work: impl FnOnce(&Store<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<StoreError>,
    {
        let tx = self
            .conn()
            .unchecked_transaction()
            .map_err(StoreError::from)?;
        #[cfg(feature = "engine-store")]
        let page = self
            .core()
            .map(super::engine::page::Page::begin)
            .transpose()?;
        let done = work(&self.sharing_engine(&tx))?;
        #[cfg(feature = "engine-store")]
        if let Some(page) = page {
            page.finish()?;
        }
        tx.commit().map_err(StoreError::from)?;
        Ok(done)
    }

    /// A store over `conn` that shares this store's engine tables: for code
    /// that runs a SQLite transaction of its own inside a store call. The
    /// engine writes are not part of that transaction; use
    /// [`Store::transaction`] for that.
    pub fn sharing_engine<'c>(&self, conn: &'c Connection) -> Store<'c> {
        let store = Store::over_sqlite(conn);
        #[cfg(feature = "engine-store")]
        let store = store.with_engine(self.engine.clone());
        store
    }

    /// Where a background thread can reopen this store: its database file
    /// and its engine tables. `None` for an in-memory database, which a
    /// second connection cannot reach.
    pub fn secondary_source(&self) -> Option<SecondarySource> {
        let path = super::database_path(self).ok().flatten()?;
        Some(SecondarySource::new(
            path,
            #[cfg(feature = "engine-store")]
            self.engine.clone(),
        ))
    }

    /// A store over a SQLite connection opened elsewhere. It has no engine
    /// tables, so every group reads that connection; a background thread
    /// uses [`SecondarySource::store`] instead, which keeps them.
    pub fn over_sqlite(conn: &'a Connection) -> Self {
        Self {
            backend: Backend::SqliteBorrowed(conn),
            #[cfg(feature = "engine-store")]
            engine: None,
        }
    }

    /// The engine tables, for the repositories moved onto them.
    #[cfg(feature = "engine-store")]
    pub(crate) fn engine(&self) -> Option<&EngineLock> {
        self.engine.as_deref()
    }

    /// The engine tables, for the memories core's repositories: every store
    /// on the engine holds the core since the switch-on (core PR 5b).
    #[cfg(feature = "engine-store")]
    pub(crate) fn core(&self) -> Option<&EngineLock> {
        self.engine()
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

/// Where a background thread reopens the store: the database file, for a
/// SQLite connection of its own, and the engine tables it shares with the
/// store it came from (ADR-0023, phase 4d).
///
/// A thread keeps its own connection so it never holds the process-wide
/// SQLite lock across slow work such as network I/O. The engine tables lock
/// per call, so sharing them keeps that property.
#[derive(Clone)]
pub struct SecondarySource {
    path: PathBuf,
    #[cfg(feature = "engine-store")]
    engine: Option<EngineHandle>,
}

impl SecondarySource {
    pub(crate) fn new(
        path: PathBuf,
        #[cfg(feature = "engine-store")] engine: Option<EngineHandle>,
    ) -> Self {
        Self {
            path,
            #[cfg(feature = "engine-store")]
            engine,
        }
    }

    /// The database file to open a connection to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A store over `conn`, a connection to [`Self::path`], with the shared
    /// engine tables beside it.
    pub fn store<'c>(&self, conn: &'c Connection) -> Store<'c> {
        let store = Store::over_sqlite(conn);
        #[cfg(feature = "engine-store")]
        let store = store.with_engine(self.engine.clone());
        store
    }
}

impl fmt::Debug for SecondarySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecondarySource")
            .field("path", &self.path)
            .finish_non_exhaustive()
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
    /// The engine failed (`engine-store` builds only).
    Engine(String),
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
            StoreError::Engine(why) => write!(f, "engine store: {why}"),
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

    /// A failed transaction, then one that succeeds, as text, on `db`.
    fn exercise_transactions(db: &super::super::Database) -> Vec<String> {
        use crate::db::imports::ImportLedger;
        use crate::db::memories::{Memories, NewMemory};
        use crate::db::vectors::Vectors;
        const NOW: &str = "2026-09-27T00:00:00+00:00";

        let store = db.store();
        Vectors::new(&store).put("m1", 0, &[1]).unwrap();
        let write = |page: &Store<'_>, fail: bool| -> Result<usize> {
            Memories::new(page).insert(&NewMemory::new("m1", "quokka", NOW))?;
            Vectors::new(page).put("m1", 0, &[2])?;
            ImportLedger::new(page).record_mempalace("d1", "m1", NOW)?;
            // The page reads its own writes.
            let seen = ImportLedger::new(page).imported_drawers(&["d1"])?.len();
            if fail {
                return Err(StoreError::Invalid("the importer gave up".to_string()));
            }
            Ok(seen)
        };
        let state = |store: &Store<'_>| {
            format!(
                "{:?} {:?} {:?}",
                Memories::new(store).exists("m1").unwrap(),
                Vectors::new(store).any_embedding().unwrap(),
                ImportLedger::new(store).imported_drawers(&["d1"]).unwrap(),
            )
        };
        let failed = store.transaction(|page| write(page, true));
        let mut seen = vec![format!("{failed:?}"), state(&store)];
        let kept = store.transaction(|page| write(page, false));
        seen.push(format!("{kept:?}"));
        seen.push(state(&store));
        seen
    }

    #[test]
    fn a_transaction_lands_whole_or_not_at_all_on_both_backends() {
        let mut observed = Vec::new();
        super::super::on_each_backend(|db| observed.push(exercise_transactions(db)));
        let sqlite = &observed[0];
        assert_eq!(
            sqlite[1], "false Some([1]) []",
            "the failed one left nothing"
        );
        assert_eq!(
            sqlite[3], "true Some([2]) [\"d1\"]",
            "the other landed whole"
        );
        for other in &observed[1..] {
            assert_eq!(other, sqlite);
        }
    }

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

    #[test]
    fn an_in_memory_database_has_no_secondary_source() {
        let db = crate::db::Database::open_in_memory().unwrap();
        assert!(db.secondary_source().is_none());
        assert!(db.store().secondary_source().is_none());
    }

    #[test]
    fn a_file_database_names_its_path() {
        let dir =
            std::env::temp_dir().join(format!("remind_me_secondary_source_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.db");
        let db = crate::db::Database::open(&path).unwrap();
        let from_db = db.secondary_source().unwrap();
        let from_store = db.store().secondary_source().unwrap();
        assert_eq!(from_db.path(), path.as_path());
        assert_eq!(from_store.path().file_name(), path.file_name());
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(feature = "engine-store")]
    mod engine {
        use super::*;
        use crate::db::saved_searches::SavedSearches;
        use crate::db::Database;
        use crate::models::{SavedSearch, SavedSearchFilters};

        fn saved(id: &str) -> SavedSearch {
            SavedSearch {
                id: id.to_string(),
                name: format!("name-{id}"),
                query: "q".to_string(),
                filters: SavedSearchFilters::default(),
                watch: false,
                created_at: "2026-09-27T00:00:00+00:00".to_string(),
                updated_at: "2026-09-27T00:00:00+00:00".to_string(),
            }
        }

        /// A background thread's store: its own SQLite connection, the
        /// database's engine tables. What it writes, the main store reads,
        /// and the other way round.
        #[test]
        fn a_secondary_store_shares_the_engine_tables() {
            let db = Database::open_in_memory_on_engine().unwrap();
            let source = SecondarySource::new(PathBuf::from("unused.db"), db.engine.clone());
            let own = Connection::open_in_memory().unwrap();

            SavedSearches::new(&source.store(&own))
                .insert(&saved("from-thread"))
                .unwrap();
            SavedSearches::new(&db.store())
                .insert(&saved("from-main"))
                .unwrap();

            let main = db.store();
            let thread = source.store(&own);
            for store in [&main, &thread] {
                let ids: Vec<String> = SavedSearches::new(store)
                    .list()
                    .unwrap()
                    .into_iter()
                    .map(|s| s.id)
                    .collect();
                assert_eq!(ids, ["from-main", "from-thread"]);
            }
        }

        /// An importer's transaction store keeps the engine tables of the
        /// store it was made from.
        #[test]
        fn a_transaction_store_shares_the_engine_tables() {
            let db = Database::open_in_memory_on_engine().unwrap();
            let other = Connection::open_in_memory().unwrap();
            {
                let store = db.store();
                let page = store.sharing_engine(&other);
                SavedSearches::new(&page).insert(&saved("in-tx")).unwrap();
            }
            assert!(SavedSearches::new(&db.store()).get("in-tx").is_ok());
        }
    }
}
