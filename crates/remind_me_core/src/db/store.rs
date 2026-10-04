//! The store handle every caller takes, and the error every repository
//! returns (ADR-0023, phase 4; ADR-0025).
//!
//! Callers used to take a SQLite connection, which tied every one of
//! them to SQLite. They take a [`Store`] instead. Until phase 6 the store
//! was an enum over two backends; the engine is the only one now, so a
//! store is the engine tables plus the file they stand beside.
//!
//! A [`Store`] made by [`crate::db::Database::store`] holds the database's
//! lock for as long as it lives, as the connection guard it replaced did:
//! one tool call runs at a time. A background thread's store
//! ([`SecondarySource::store`]) holds no such lock; the engine tables lock
//! per call, which is what keeps a long sync cycle from stalling every
//! other caller.

use super::engine::{page::Page, EngineHandle, EngineLock};
use parking_lot::MutexGuard;
use std::fmt;
use std::path::{Path, PathBuf};

/// A handle on the node's store.
pub struct Store<'a> {
    engine: EngineHandle,
    /// The database file the engine directory stands beside; `None` for an
    /// in-memory store.
    path: Option<PathBuf>,
    /// The process-wide lock, when this store came from the database.
    _lock: Option<MutexGuard<'a, ()>>,
}

impl<'a> Store<'a> {
    pub(crate) fn locked(
        guard: MutexGuard<'a, ()>,
        engine: EngineHandle,
        path: Option<PathBuf>,
    ) -> Self {
        Self {
            engine,
            path,
            _lock: Some(guard),
        }
    }

    /// A store over shared engine tables, holding no process lock.
    pub(crate) fn unlocked(engine: EngineHandle, path: Option<PathBuf>) -> Store<'static> {
        Store {
            engine,
            path,
            _lock: None,
        }
    }

    /// Run `work` as one transaction: every write it makes lands, or none
    /// does. The writes are one page on the engine (`db::engine::page`),
    /// readable as they go and finished when `work` returns.
    ///
    /// An error from `work`, or a panic, abandons the page.
    ///
    /// # Errors
    ///
    /// Whatever `work` returns, or [`StoreError`] if the page cannot be
    /// opened or finished.
    pub fn transaction<T, E>(
        &self,
        work: impl FnOnce(&Store<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<StoreError>,
    {
        let page = Page::begin(&self.engine)?;
        let done = work(self)?;
        page.finish()?;
        Ok(done)
    }

    /// Where a background thread can reopen this store. `None` for an
    /// in-memory database, which exists only as long as its `Database`.
    pub fn secondary_source(&self) -> Option<SecondarySource> {
        Some(SecondarySource::new(
            self.path.clone()?,
            self.engine.clone(),
        ))
    }

    /// The database file this store stands beside, or `None` in memory.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The engine tables.
    pub(crate) fn engine(&self) -> &EngineLock {
        &self.engine
    }

    /// The engine tables, for the memories core's repositories. The same
    /// tables as [`Store::engine`]; the name says which group a caller is
    /// after.
    pub(crate) fn core(&self) -> &EngineLock {
        &self.engine
    }
}

/// Where a background thread reopens the store: the database file, and
/// the engine tables it shares with the store it came from (ADR-0023,
/// phase 4d).
///
/// The engine tables lock per call, so a thread that holds no process
/// lock never holds the store across slow work such as network I/O.
#[derive(Clone)]
pub struct SecondarySource {
    path: PathBuf,
    engine: EngineHandle,
}

impl SecondarySource {
    pub(crate) fn new(path: PathBuf, engine: EngineHandle) -> Self {
        Self { path, engine }
    }

    /// The database file the store stands beside.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A store over the shared engine tables, holding no process lock.
    pub fn store(&self) -> Store<'static> {
        Store::unlocked(self.engine.clone(), Some(self.path.clone()))
    }
}

impl fmt::Debug for SecondarySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecondarySource")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Why a store operation failed.
#[derive(Debug)]
pub enum StoreError {
    /// The row asked for does not exist.
    NotFound,
    /// The request itself was not valid: a bad argument, not a failure of
    /// the store.
    Invalid(String),
    /// The legacy SQLite reader failed while copying an old `memory.db`
    /// onto the engine (`db::legacy_sqlite`).
    Legacy(String),
    /// The engine failed.
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
            StoreError::Legacy(why) => write!(f, "legacy SQLite store: {why}"),
            StoreError::Engine(why) => write!(f, "engine store: {why}"),
        }
    }
}

impl std::error::Error for StoreError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::saved_searches::SavedSearches;
    use crate::db::Database;
    use crate::models::{SavedSearch, SavedSearchFilters};

    /// A failed transaction, then one that succeeds, as text, on `db`.
    fn exercise_transactions(db: &Database) -> Vec<String> {
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
    fn a_transaction_lands_whole_or_not_at_all() {
        let db = Database::open_in_memory().unwrap();
        let seen = exercise_transactions(&db);
        assert!(seen[0].contains("the importer gave up"), "{}", seen[0]);
        assert_eq!(seen[1], "false Some([1]) []", "the failed one left nothing");
        assert_eq!(seen[2], "Ok(1)");
        assert_eq!(
            seen[3], "true Some([2]) [\"d1\"]",
            "the other landed whole"
        );
    }

    #[test]
    fn not_found_keeps_the_message_callers_match_on() {
        assert_eq!(StoreError::NotFound.to_string(), "Query returned no rows");
        assert!(StoreError::Legacy("x".into()).to_string().contains("legacy"));
    }

    #[test]
    fn an_in_memory_database_has_no_secondary_source() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.secondary_source().is_none());
        assert!(db.store().secondary_source().is_none());
        assert!(db.store().path().is_none());
    }

    #[test]
    fn a_file_database_names_its_path() {
        let dir =
            std::env::temp_dir().join(format!("remind_me_secondary_source_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.db");
        let db = Database::open(&path).unwrap();
        let from_db = db.secondary_source().unwrap();
        let from_store = db.store().secondary_source().unwrap();
        assert_eq!(from_db.path(), path.as_path());
        assert_eq!(from_store.path(), path.as_path());
        assert_eq!(db.store().path(), Some(path.as_path()));
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

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

    /// A background thread's store shares the database's engine tables:
    /// what it writes, the main store reads, and the other way round.
    #[test]
    fn a_secondary_store_shares_the_engine_tables() {
        let db = Database::open_in_memory().unwrap();
        let source = SecondarySource::new(PathBuf::from("unused.db"), db.engine_handle());

        SavedSearches::new(&source.store())
            .insert(&saved("from-thread"))
            .unwrap();
        SavedSearches::new(&db.store())
            .insert(&saved("from-main"))
            .unwrap();

        let main = db.store();
        let thread = source.store();
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
}
