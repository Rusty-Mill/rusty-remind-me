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
mod dialogs;
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

pub use dialogs::{SqliteDialogs, FILE as DIALOGS_FILE};

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
    /// Set when a commit, rollback or savepoint did not settle: only
    /// reopening makes the store safe.
    failed: bool,
    /// Set when a savepoint inside an open transaction could not be put
    /// back: that transaction may hold half of it, so it must not commit.
    poisoned: bool,
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
        conn.busy_timeout(std::time::Duration::ZERO)
            .map_err(sqlite_error)?;
        // One process owns a node's store, as on the engine: the daemon
        // and its fallback rely on a second opener being refused. Exclusive
        // locking, set before WAL, holds the file lock from here on.
        conn.execute_batch(
            "PRAGMA locking_mode = EXCLUSIVE; PRAGMA journal_mode = WAL; \
             PRAGMA synchronous = FULL; BEGIN EXCLUSIVE; COMMIT;",
        )
        .map_err(held_error)?;
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
            poisoned: false,
        })
    }

    /// Run `f` in a savepoint: its writes land together or not at all,
    /// nested inside an open transaction if there is one.
    ///
    /// Settled by hand, because rusqlite's `Savepoint` drop swallows
    /// cleanup errors. Outside a transaction the final `RELEASE` is the
    /// commit, so a failed one is indeterminate and marks the store
    /// `NeedsRecovery`; so does a rollback that cannot be confirmed. Inside
    /// one, a savepoint that cannot be put back poisons the transaction,
    /// which then refuses to commit.
    pub(crate) fn atomic<T>(&mut self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.refuse_unsettled()?;
        if self.poisoned {
            return Err(StoreError::Engine(
                "sqlite: a write in this transaction could not be put back; it must roll back"
                    .into(),
            ));
        }
        let outermost = self.conn.is_autocommit();
        self.conn
            .execute_batch("SAVEPOINT atomic")
            .map_err(sqlite_error)?;
        let value = match f(&self.conn) {
            Ok(value) => value,
            Err(e) => {
                self.undo_savepoint(outermost);
                return Err(e);
            }
        };
        if let Err(e) = self.conn.execute_batch("RELEASE atomic") {
            if outermost {
                self.failed = true;
            } else {
                self.undo_savepoint(false);
            }
            return Err(sqlite_error(e));
        }
        Ok(value)
    }

    /// Put back the open savepoint `atomic`, and say so if that fails.
    ///
    /// Outside a transaction, a rollback that cannot be confirmed is
    /// indeterminate. Inside one, an outer transaction that SQLite has
    /// already ended (an I/O or full-disk error rolls it back whole) is
    /// too: its earlier writes are gone, and the next write would commit
    /// on its own. Only a savepoint that cannot be put back while the
    /// transaction is still open merely poisons it.
    fn undo_savepoint(&mut self, outermost: bool) {
        let undone = self
            .conn
            .execute_batch("ROLLBACK TO atomic; RELEASE atomic");
        let autocommit = self.conn.is_autocommit();
        if undone.is_ok() && autocommit == outermost {
            return;
        }
        if outermost || autocommit {
            self.failed = true;
        } else {
            self.poisoned = true;
        }
    }

    /// Refuse once an earlier write did not settle.
    fn refuse_unsettled(&self) -> Result<()> {
        if self.failed {
            return Err(StoreError::Engine(
                "sqlite: an earlier write did not settle; reopen the store".into(),
            ));
        }
        Ok(())
    }
}

/// SQLite as a whole store (ADR-0035, step 6): a transaction is one
/// `BEGIN IMMEDIATE`, so a second writer waits and readers in other
/// connections see it whole or not at all.
impl Backend for SqliteTables {
    fn begin_transaction(&mut self) -> Result<()> {
        self.refuse_unsettled()?;
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(sqlite_error)
    }

    /// A failed `COMMIT` that leaves the transaction open (`SQLITE_BUSY`,
    /// a deferred constraint) is state 3 and gets rolled back. One that
    /// leaves the connection in autocommit was an I/O error SQLite rolled
    /// back in memory, but a whole frame may still be in the WAL for
    /// recovery to replay: state 4, so the lock fails closed.
    fn commit_transaction(&mut self) -> Result<()> {
        if self.poisoned {
            return Err(StoreError::Engine(
                "sqlite: a write in this transaction could not be put back; it must roll back"
                    .into(),
            ));
        }
        let committed = self.conn.execute_batch("COMMIT").map_err(sqlite_error);
        if committed.is_err() && self.conn.is_autocommit() {
            // SQLite rolled back in memory, but an I/O error (a failed WAL
            // sync, say) can leave a whole commit frame that recovery
            // replays on reopen: the outcome is not known.
            self.failed = true;
        }
        committed
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
        self.poisoned = false;
        rolled
    }

    fn rebuild_indexes(&mut self) -> Result<()> {
        self.atomic(memories::rebuild)?;
        self.rebuild_wiki_index()
    }

    /// `VACUUM` when a quarter or more of the file is free pages, then fold
    /// the WAL into the file. Counts each that did work (the WAL as the
    /// engine counts a folded log). Skipped inside a transaction.
    fn compact(&mut self) -> Result<usize> {
        if !self.conn.is_autocommit() {
            return Ok(0);
        }
        let (free, total): (i64, i64) = self
            .one(
                "SELECT freelist_count, page_count FROM pragma_freelist_count(), pragma_page_count()",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?
            .unwrap_or((0, 0));
        let vacuum = total > 0 && free * 4 >= total;
        if vacuum {
            self.conn.execute_batch("VACUUM").map_err(sqlite_error)?;
        }
        // `TRUNCATE` reports no frames, so `PASSIVE` counts them first.
        let folded: i64 = self
            .one("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get(1))?
            .unwrap_or(0);
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(sqlite_error)?;
        Ok(usize::from(folded > 0) + usize::from(vacuum))
    }

    fn test_rows(&mut self) -> Option<&mut dyn crate::testing::TestRows> {
        Some(self)
    }

    /// A consistent copy by `VACUUM INTO`, as a directory [`Self::open`]
    /// opens.
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
    /// Run one write statement; how many rows it changed.
    ///
    /// It runs in [`Self::atomic`], so a standalone write gets the same
    /// settlement as any other: outside a transaction its implicit commit
    /// is an explicit `RELEASE`, and one that fails marks the store
    /// `NeedsRecovery` rather than leaving it `Closed`.
    pub(crate) fn exec(&mut self, sql: &str, params: impl Params) -> Result<usize> {
        self.atomic(|c| exec(c, sql, params))
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

/// The error for a store another connection holds, worded as the engine's
/// so the daemon's fallback names the same remedy.
fn held_error(e: rusqlite::Error) -> StoreError {
    if e.sqlite_error_code() != Some(rusqlite::ErrorCode::DatabaseBusy) {
        return sqlite_error(e);
    }
    StoreError::Engine(format!(
        "sqlite: {e}. Another rusty-remind-me process holds this node's store, usually the \
         store daemon; `rusty-remind-me daemon stop` releases it"
    ))
}

/// A SQLite failure as the store's error. `Engine` names the storage
/// backend that failed; adding a variant would change the public enum.
pub(crate) fn sqlite_error(e: rusqlite::Error) -> StoreError {
    StoreError::Engine(format!("sqlite: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::backend::{BackendGuard, BackendLock, Transaction};
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
    fn a_second_opener_is_refused_until_the_first_closes() {
        let dir = scratch("held");
        let first = SqliteTables::open(&dir).unwrap();
        let refused = SqliteTables::open(&dir).err().unwrap().to_string();
        assert!(refused.contains("daemon stop"), "{refused}");
        drop(first);
        SqliteTables::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn size_on_disk_counts_the_wal() {
        use crate::db::stats::StatsStore;
        let dir = scratch("size");
        let mut t = SqliteTables::open(&dir).unwrap();
        for i in 0..50 {
            MemoryStore::insert(
                &mut t,
                &NewMemory::new(format!("m{i}"), "x".repeat(2000), NOW),
            )
            .unwrap();
        }
        let on_disk: u64 = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert!(
            std::fs::metadata(dir.join(format!("{FILE}-wal")))
                .unwrap()
                .len()
                > 0
        );
        assert_eq!(t.size_on_disk().unwrap(), on_disk);
        drop(t);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A standalone write whose savepoint cleanup fails (here the write
    /// released it early, so the final `RELEASE` cannot find it) leaves
    /// the store `NeedsRecovery`, and a database on it refuses every
    /// caller after.
    #[test]
    fn a_standalone_savepoint_that_does_not_settle_fails_closed() {
        let mut t = SqliteTables::open_in_memory().unwrap();
        let unsettled = t.atomic(|c| exec(c, "RELEASE atomic", []).map(drop));
        assert!(unsettled.is_err());
        assert_eq!(t.transaction_state(), TransactionState::NeedsRecovery);
        assert!(t.atomic(|_| Ok(())).is_err(), "later writes are refused");

        let db = Database::open_backend(Box::new(SqliteTables::open_in_memory().unwrap()), None)
            .unwrap();
        let store = db.store();
        let failed = {
            let mut guard = store.backend().lock().unwrap();
            let backend: &mut dyn std::any::Any = &mut *guard;
            let sqlite = backend.downcast_mut::<SqliteTables>().unwrap();
            sqlite.atomic(|c| {
                exec(c, "ROLLBACK TO atomic", [])?;
                exec(c, "RELEASE atomic", [])?;
                Err::<(), _>(StoreError::Invalid("give up".into()))
            })
        };
        assert!(failed.is_err());
        assert!(
            Memories::new(&store).exists("m").is_err(),
            "the lock refuses reads once the store needs recovery"
        );
    }

    /// A standalone `exec` settles like `atomic`: a statement that fails
    /// is put back and the store stays usable, and a commit that cannot be
    /// confirmed (here the statement releases the savepoint, so the final
    /// `RELEASE` fails) leaves it `NeedsRecovery`.
    #[test]
    fn a_standalone_exec_settles_like_atomic() {
        let mut t = SqliteTables::open_in_memory().unwrap();
        assert!(t.exec("INSERT INTO no_such_table VALUES (1)", []).is_err());
        assert_eq!(t.transaction_state(), TransactionState::Closed);
        MemoryStore::insert(&mut t, &NewMemory::new("m", "x", NOW)).unwrap();

        assert!(t.exec("RELEASE atomic", []).is_err());
        assert_eq!(t.transaction_state(), TransactionState::NeedsRecovery);
        assert!(
            t.exec("DELETE FROM memories", []).is_err(),
            "later writes are refused"
        );
    }

    /// The SQLite tables behind a lock, for a test that pokes them.
    fn sqlite<'g>(guard: &'g mut BackendGuard<'_>) -> &'g mut SqliteTables {
        let backend: &mut dyn std::any::Any = &mut **guard;
        backend.downcast_mut::<SqliteTables>().unwrap()
    }

    /// A transaction cannot begin on a store that a standalone write left
    /// unsettled, and the lock stays failed closed.
    #[test]
    fn a_new_transaction_is_refused_after_an_unsettled_standalone_write() {
        let lock = BackendLock::new(Box::new(SqliteTables::open_in_memory().unwrap()));
        {
            let mut guard = lock.lock().unwrap();
            assert!(sqlite(&mut guard).exec("RELEASE atomic", []).is_err());
        }
        assert!(Transaction::begin(&lock).is_err());
        assert!(lock.lock().is_err(), "the lock refuses every caller after");

        let mut t = SqliteTables::open_in_memory().unwrap();
        t.failed = true;
        assert!(t.begin_transaction().is_err(), "the backend refuses too");
    }

    /// A commit that SQLite reports as done is still an error when the
    /// backend is left unsettled.
    #[test]
    fn a_commit_that_leaves_the_store_unsettled_is_an_error() {
        let lock = BackendLock::new(Box::new(SqliteTables::open_in_memory().unwrap()));
        let tx = Transaction::begin(&lock).unwrap();
        sqlite(&mut lock.lock().unwrap()).failed = true;
        assert!(tx.commit().is_err());
        assert!(lock.lock().is_err());
    }

    /// An outer transaction that SQLite ended inside a savepoint leaves
    /// the store `NeedsRecovery`: the next write must not commit alone.
    #[test]
    fn a_lost_outer_transaction_refuses_further_writes() {
        let mut t = SqliteTables::open_in_memory().unwrap();
        t.begin_transaction().unwrap();
        MemoryStore::insert(&mut t, &NewMemory::new("gone", "x", NOW)).unwrap();
        let lost = t.atomic(|c| {
            exec(c, "ROLLBACK", [])?;
            Err::<(), _>(StoreError::Invalid("give up".into()))
        });
        assert!(lost.is_err());
        assert_eq!(t.transaction_state(), TransactionState::NeedsRecovery);
        assert!(MemoryStore::insert(&mut t, &NewMemory::new("alone", "x", NOW)).is_err());
        assert!(t.exec("DELETE FROM memories", []).is_err());
        assert!(!MemoryStore::exists(&t, "alone").unwrap());
    }

    /// The same loss through the shared wrapper: the owning thread's own
    /// reads and writes are refused before it leaves the transaction, the
    /// transaction returns an error even though its work returned Ok, and
    /// the store refuses everyone until it is reopened.
    #[test]
    fn a_lost_outer_transaction_is_refused_to_its_owner_until_reopen() {
        let dir = scratch("lost_outer");
        let db = Database::open_backend(Box::new(SqliteTables::open(&dir).unwrap()), None).unwrap();
        let store = db.store();
        let outcome = store.transaction(|store| {
            MemoryStore::insert(
                &mut *store.backend().lock()?,
                &NewMemory::new("gone", "x", NOW),
            )?;
            let lost = sqlite(&mut store.backend().lock()?).atomic(|c| {
                exec(c, "ROLLBACK", [])?;
                Err::<(), _>(StoreError::Invalid("give up".into()))
            });
            assert!(lost.is_err());
            assert!(
                Memories::new(store).exists("gone").is_err(),
                "the owner's reads are refused"
            );
            assert!(store.backend().lock().is_err(), "and so are its writes");
            Ok::<(), StoreError>(())
        });
        assert!(outcome.is_err(), "the transaction reports the loss");
        assert!(Memories::new(&store).exists("gone").is_err());
        assert!(store.transaction(|_| Ok::<(), StoreError>(())).is_err());
        drop(store);
        drop(db);

        let db = Database::open_backend(Box::new(SqliteTables::open(&dir).unwrap()), None).unwrap();
        let store = db.store();
        assert!(!Memories::new(&store).exists("gone").unwrap(), "reopened");
        drop(store);
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A poisoned transaction refuses further writes, not only its commit.
    #[test]
    fn a_poisoned_transaction_refuses_further_writes() {
        let mut t = SqliteTables::open_in_memory().unwrap();
        t.begin_transaction().unwrap();
        t.poisoned = true;
        assert!(MemoryStore::insert(&mut t, &NewMemory::new("m", "x", NOW)).is_err());
        t.rollback_transaction().unwrap();
        MemoryStore::insert(&mut t, &NewMemory::new("m", "x", NOW)).unwrap();
    }

    /// Inside a transaction, a savepoint that cannot be put back poisons
    /// it: the commit is refused, and the rollback that follows clears it.
    #[test]
    fn a_nested_savepoint_that_cannot_be_put_back_refuses_the_commit() {
        let mut t = SqliteTables::open_in_memory().unwrap();
        t.begin_transaction().unwrap();
        MemoryStore::insert(&mut t, &NewMemory::new("kept", "x", NOW)).unwrap();
        let failed = t.atomic(|c| {
            exec(c, "ROLLBACK TO atomic", [])?;
            exec(c, "RELEASE atomic", [])?;
            Err::<(), _>(StoreError::Invalid("give up".into()))
        });
        assert!(failed.is_err());
        assert_eq!(t.transaction_state(), TransactionState::Open);
        assert!(
            t.commit_transaction().is_err(),
            "a poisoned transaction does not commit"
        );
        t.rollback_transaction().unwrap();
        assert_eq!(t.transaction_state(), TransactionState::Closed);
        assert!(!MemoryStore::exists(&t, "kept").unwrap());
        t.begin_transaction().unwrap();
        MemoryStore::insert(&mut t, &NewMemory::new("next", "x", NOW)).unwrap();
        t.commit_transaction().unwrap();
        assert!(MemoryStore::exists(&t, "next").unwrap());
    }

    /// A write that fails inside a transaction is put back on its own:
    /// the transaction's other writes still commit.
    #[test]
    fn a_failed_write_inside_a_transaction_is_put_back_alone() {
        let mut t = SqliteTables::open_in_memory().unwrap();
        t.begin_transaction().unwrap();
        MemoryStore::insert(&mut t, &NewMemory::new("kept", "x", NOW)).unwrap();
        let failed = t.atomic(|c| {
            exec(c, "DELETE FROM memories", [])?;
            Err::<(), _>(StoreError::Invalid("give up".into()))
        });
        assert!(failed.is_err());
        t.commit_transaction().unwrap();
        assert!(MemoryStore::exists(&t, "kept").unwrap());
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
    fn compact_folds_the_wal_and_vacuums_only_a_mostly_free_file() {
        let dir = scratch("compact");
        let mut t = SqliteTables::open(&dir).unwrap();
        assert_eq!(t.compact().unwrap(), 1, "the schema's writes folded");
        assert_eq!(t.compact().unwrap(), 0, "nothing written since");
        let body = "x".repeat(4096);
        for i in 0..200 {
            MemoryStore::insert(&mut t, &NewMemory::new(format!("m{i}"), &body, NOW)).unwrap();
        }
        t.exec("DELETE FROM memories", []).unwrap();
        assert_eq!(t.compact().unwrap(), 2, "folded and vacuumed");
        assert_eq!(t.compact().unwrap(), 0);
        drop(t);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
