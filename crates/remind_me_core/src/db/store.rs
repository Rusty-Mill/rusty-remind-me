//! The store handle every caller takes, and the error every repository
//! returns (ADR-0023, phase 4; ADR-0025).
//!
//! Callers used to take a SQLite connection, which tied every one of
//! them to SQLite. They take a [`Store`] instead: a handle on a
//! [`Backend`](super::backend::Backend), the supertrait of every table
//! group's port (ADR-0035, step 6), plus the file it stands beside. The
//! engine is the only backend in production.
//!
//! A [`Store`] made by [`crate::db::Database::store`] holds the database's
//! lock for as long as it lives, as the connection guard it replaced did:
//! one tool call runs at a time. A background thread's store
//! ([`SecondarySource::store`]) holds no such lock; the backend locks
//! per call, which is what keeps a long sync cycle from stalling every
//! other caller.

use super::backend::{BackendHandle, BackendLock, Transaction};
use parking_lot::MutexGuard;
use std::fmt;
use std::path::{Path, PathBuf};

/// A handle on the node's store.
pub struct Store<'a> {
    backend: BackendHandle,
    /// The database file the store stands beside; `None` for an in-memory
    /// store.
    path: Option<PathBuf>,
    /// The process-wide lock, when this store came from the database.
    _lock: Option<MutexGuard<'a, ()>>,
}

impl<'a> Store<'a> {
    pub(crate) fn locked(
        guard: MutexGuard<'a, ()>,
        backend: BackendHandle,
        path: Option<PathBuf>,
    ) -> Self {
        Self {
            backend,
            path,
            _lock: Some(guard),
        }
    }

    /// A store over a shared backend, holding no process lock.
    pub(crate) fn unlocked(backend: BackendHandle, path: Option<PathBuf>) -> Store<'static> {
        Store {
            backend,
            path,
            _lock: None,
        }
    }

    /// Run `work` as one transaction: every write it makes lands, or none
    /// does. The writes are readable as they go and land when `work`
    /// returns (on the engine, as one page: `db::engine::page`).
    ///
    /// An error from `work`, or a panic, rolls the transaction back.
    ///
    /// # Errors
    ///
    /// Whatever `work` returns, or [`StoreError`] if the transaction cannot
    /// be opened or committed.
    pub fn transaction<T, E>(
        &self,
        work: impl FnOnce(&Store<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<StoreError>,
    {
        let tx = Transaction::begin(&self.backend)?;
        let done = work(self)?;
        tx.commit()?;
        Ok(done)
    }

    /// Run `work` atomically: inside the transaction this thread already
    /// has open, whose outcome its writes then share, or else as a
    /// transaction of its own ([`Store::transaction`]). For a write that
    /// must never land in part but may be called from inside a larger one
    /// (transactions do not nest). Joined, an error from `work` is undone
    /// only if the caller passes it up so the outer transaction rolls
    /// back: a caller that swallows it and commits keeps the part written.
    ///
    /// # Errors
    ///
    /// As [`Store::transaction`].
    pub(crate) fn atomically<T, E>(
        &self,
        work: impl FnOnce(&Store<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<StoreError>,
    {
        if self.backend.held_here() {
            return work(self);
        }
        self.transaction(work)
    }

    /// Where a background thread can reopen this store. `None` for an
    /// in-memory database, which exists only as long as its `Database`.
    pub fn secondary_source(&self) -> Option<SecondarySource> {
        Some(SecondarySource::new(
            self.path.clone()?,
            self.backend.clone(),
        ))
    }

    /// The database file this store stands beside, or `None` in memory.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The backend every repository goes through.
    pub(crate) fn backend(&self) -> &BackendLock {
        &self.backend
    }
}

/// Where a background thread reopens the store: the database file, and
/// the backend it shares with the store it came from (ADR-0023, phase 4d).
///
/// The backend locks per call, so a thread that holds no process
/// lock never holds the store across slow work such as network I/O.
#[derive(Clone)]
pub struct SecondarySource {
    path: PathBuf,
    backend: BackendHandle,
}

impl SecondarySource {
    pub(crate) fn new(path: PathBuf, backend: BackendHandle) -> Self {
        Self { path, backend }
    }

    /// The database file the store stands beside.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A store over the shared backend, holding no process lock.
    pub fn store(&self) -> Store<'static> {
        Store::unlocked(self.backend.clone(), Some(self.path.clone()))
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
        assert_eq!(seen[3], "true Some([2]) [\"d1\"]", "the other landed whole");
    }

    #[test]
    fn atomically_opens_a_transaction_or_joins_the_open_one() {
        use crate::db::memories::{Memories, NewMemory};
        use crate::db::queries::get_memory_by_id;
        const NOW: &str = "2026-11-23T00:00:00+00:00";
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let insert =
            |s: &Store<'_>, id: &str| Memories::new(s).insert(&NewMemory::new(id, "x", NOW));
        let there = |id: &str| get_memory_by_id(&store, id).unwrap().is_some();

        // On its own: a transaction of its own, rolled back by its error.
        let alone: Result<()> = store.atomically(|s| {
            insert(s, "alone")?;
            Err(StoreError::Invalid("gave up".into()))
        });
        assert!(alone.is_err());
        assert!(!there("alone"));

        // Inside a transaction: no nesting error, and the outer outcome
        // decides.
        let mut joined = None;
        let outer: Result<()> = store.transaction(|s| {
            joined = Some(s.atomically(|s| insert(s, "inner")).is_ok());
            Err(StoreError::Invalid("outer gave up".into()))
        });
        assert!(outer.is_err());
        assert_eq!(joined, Some(true));
        assert!(!there("inner"));
        store
            .transaction(|s| s.atomically(|s| insert(s, "kept")))
            .unwrap();
        assert!(there("kept"));
    }

    /// An update and a delete join the caller's transaction (Q21): rolled
    /// back with it, they leave the row and its revisions as they were.
    #[test]
    fn an_update_and_a_delete_share_the_callers_transaction() {
        use crate::db::history::Revisions;
        use crate::db::queries::{add_memory, delete_memory, get_memory_by_id, update_memory};
        use crate::models::{MemoryAddInput, MemoryUpdateInput};
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let input = MemoryAddInput {
            content: "first".into(),
            category: "fact".into(),
            ..MemoryAddInput::default()
        };
        let id = add_memory(&store, input).unwrap().id;
        let edit: MemoryUpdateInput =
            serde_json::from_value(serde_json::json!({ "memory_id": id, "content": "second" }))
                .unwrap();
        let undone: Result<()> = store.transaction(|s| {
            update_memory(s, &edit)?;
            assert_eq!(
                Revisions::new(s).list(&id, 10)?.len(),
                1,
                "readable as it goes"
            );
            delete_memory(s, &id)?;
            Err(StoreError::Invalid("rolled back".into()))
        });
        assert!(undone.is_err());
        let memory = get_memory_by_id(&store, &id).unwrap().unwrap();
        assert_eq!(memory.content, "first");
        assert!(Revisions::new(&store).list(&id, 10).unwrap().is_empty());
        // On their own they still land.
        update_memory(&store, &edit).unwrap();
        assert_eq!(Revisions::new(&store).list(&id, 10).unwrap().len(), 1);
        assert!(delete_memory(&store, &id).unwrap());
        assert!(Revisions::new(&store).list(&id, 10).unwrap().is_empty());
    }

    #[test]
    fn not_found_keeps_the_message_callers_match_on() {
        assert_eq!(StoreError::NotFound.to_string(), "Query returned no rows");
        assert!(StoreError::Legacy("x".into())
            .to_string()
            .contains("legacy"));
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

    /// A background thread's store shares the database's backend:
    /// what it writes, the main store reads, and the other way round.
    #[test]
    fn a_secondary_store_shares_the_backend() {
        let db = Database::open_in_memory().unwrap();
        let source = SecondarySource::new(PathBuf::from("unused.db"), db.backend_handle());

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
