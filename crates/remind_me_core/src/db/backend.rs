//! The backend a [`super::Store`] holds: every storage port in one handle,
//! plus the few operations that span them (ADR-0035, step 6).
//!
//! Each table group's port is declared beside its repository. [`Backend`]
//! is their supertrait, so a type that implements every port and the
//! whole-store operations here can back a running
//! [`super::Database`] ([`super::Database::open_backend`]). The engine is
//! the only backend so far (`db::engine`); R3 adds others to measure them
//! on the same suites.
//!
//! [`BackendLock`] is the backend behind its lock, shared by every thread
//! that uses it, with the transaction gate in front: while one thread holds
//! a transaction open, every other thread waits at [`BackendLock::lock`],
//! as a second SQLite writer waits for a write transaction.

use super::archives::ArchiveStore;
use super::curation::CurationStore;
use super::entities::EntityStore;
use super::feedback::FeedbackStore;
use super::history::HistoryStore;
use super::imports::ImportStore;
use super::memories::MemoryStore;
use super::outbox::OutboxStore;
use super::promotions::PromotionStore;
use super::references::ReferenceStore;
use super::related::RelatedStore;
use super::reminders::ReminderStore;
use super::saved_searches::SavedSearchStore;
use super::sessions::SessionStore;
use super::stats::{SnapshotStore, StatsStore};
use super::sync_state::SyncStateStore;
use super::vectors::VectorStore;
use super::wiki::WikiStore;
use super::{Result, StoreError};
use parking_lot::{Condvar, MappedMutexGuard, Mutex, MutexGuard};
use std::any::Any;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::thread::ThreadId;

/// A whole store: every table group's port, and what spans them.
///
/// `Any` lets engine-only code (the engine's own tests and copy) find the
/// engine behind a handle and refuse plainly on any other backend.
pub trait Backend:
    ArchiveStore
    + CurationStore
    + EntityStore
    + FeedbackStore
    + HistoryStore
    + ImportStore
    + MemoryStore
    + OutboxStore
    + PromotionStore
    + ReferenceStore
    + RelatedStore
    + ReminderStore
    + SavedSearchStore
    + SessionStore
    + StatsStore
    + SnapshotStore
    + SyncStateStore
    + VectorStore
    + WikiStore
    + Send
    + Any
{
    /// Open a transaction for the calling thread. Its writes are readable
    /// at once, by this thread, and land together at
    /// [`Self::commit_transaction`] or not at all. Transactions do not
    /// nest; [`Transaction::begin`] refuses a nested one before it gets
    /// here.
    ///
    /// # Errors
    ///
    /// The backend's error if it refuses writes or one is already open.
    fn begin_transaction(&mut self) -> Result<()>;

    /// Make every write of the open transaction durable, as one.
    ///
    /// **When it fails**, the backend must leave one of these states, and
    /// nothing else, before it returns:
    ///
    /// 1. rolled back: none of the transaction's writes is visible;
    /// 2. committed: all of them are visible and will survive a reopen,
    ///    and only housekeeping after the commit point failed;
    /// 3. still open ([`TransactionState::Open`]): the caller then calls
    ///    [`Self::rollback_transaction`];
    /// 4. indeterminate or unsettled ([`TransactionState::NeedsRecovery`]):
    ///    the outcome is not known, or cleanup failed; reopening the
    ///    backend settles the transaction whole one way or the other, and
    ///    the evidence it needs (logs, journals) is left in place.
    ///
    /// The wrapper does not take an error to mean the transaction closed.
    /// It holds the gate, rolls back in state 3, and lets other threads in
    /// only once [`Self::transaction_state`] is
    /// [`TransactionState::Closed`]. Anything else fails the
    /// [`BackendLock`] closed: every later read and write, on every thread,
    /// is refused until the database is reopened. So other threads see the
    /// whole batch or none of it, never unsettled state.
    ///
    /// # Errors
    ///
    /// The backend's error if none is open, a write in it failed, or it
    /// cannot be made durable.
    fn commit_transaction(&mut self) -> Result<()>;

    /// Where this backend's transaction stands.
    fn transaction_state(&self) -> TransactionState;

    /// Put back everything the open transaction wrote.
    ///
    /// # Errors
    ///
    /// The backend's error if none is open or the rollback fails. A failed
    /// rollback must leave [`TransactionState::NeedsRecovery`] (or still
    /// [`TransactionState::Open`]), never `Closed` over half-restored rows.
    fn rollback_transaction(&mut self) -> Result<()>;

    /// Rebuild every derived index (full-text, tags) from the stored rows.
    /// Always safe: nothing derived is a source of truth.
    ///
    /// # Errors
    ///
    /// The backend's error if a row cannot be read or an index written.
    fn rebuild_indexes(&mut self) -> Result<()>;

    /// Reclaim space wherever there is enough to be worth it. How many
    /// tables (or files) were compacted; `0` when there was nothing to do.
    ///
    /// # Errors
    ///
    /// The backend's error; a failure must leave the store readable.
    fn compact(&mut self) -> Result<usize>;

    /// Write a copy of the whole store to `dest`, which must not exist yet,
    /// in a form this backend opens as it would after a clean shutdown.
    ///
    /// # Errors
    ///
    /// The backend's error naming what could not be copied.
    fn backup_to(&self, dest: &Path) -> Result<()>;

    /// What the refinement flag reads (D104), in one call:
    /// - `settled_captures`: how many distinct `capture_id`s
    ///   [`CurationStore::undecomposed`] lists rows for (sessions, not rows);
    /// - `last_capture_at`: as [`CurationStore::capture_activity`] gives it;
    /// - `last_run`: the newest live memory of `run_category` by
    ///   `(created_at, id)`, sensitive or not, as `(id, created_at)`.
    ///
    /// The default composes those existing reads
    /// ([`super::curation::observe_by_parts`]), so any backend has it; the
    /// engine and SQLite override it with one pass and narrow reads.
    ///
    /// # Errors
    ///
    /// The backend's error from any of those reads.
    fn refine_observation(
        &self,
        settled_before: Option<&str>,
        run_category: &str,
    ) -> Result<super::curation::RefineObservation> {
        super::curation::observe_by_parts(self, settled_before, run_category)
    }

    /// This backend's raw rows, for `crate::testing`'s helpers (D42).
    /// `None`, the default, makes those helpers refuse.
    #[doc(hidden)]
    fn test_rows(&mut self) -> Option<&mut dyn crate::testing::TestRows> {
        None
    }
}

/// Where a backend's transaction stands ([`Backend::transaction_state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionState {
    /// None open, and every earlier one settled: safe for any thread.
    Closed,
    /// One is open.
    Open,
    /// An earlier one did not settle (an indeterminate commit, a failed
    /// rollback): only reopening the backend makes it safe to read again.
    NeedsRecovery,
}

/// The backend shared by the database, its stores, and the background
/// threads that work beside it.
pub type BackendHandle = Arc<BackendLock>;

/// The backend, locked for one call. Derefs to `dyn Backend`, so a port
/// method is called on it directly.
pub type BackendGuard<'a> = MappedMutexGuard<'a, dyn Backend>;

/// The backend behind its lock, and the transaction gate in front of it.
///
/// While one thread holds a transaction open, every other thread's
/// [`Self::lock`] waits until it is committed or rolled back: its reads
/// and writes then see the transaction whole or not at all. The
/// transaction's own thread passes straight through.
///
/// A transaction that does not settle cleanly fails the lock closed (see
/// [`Backend::commit_transaction`]): from then on [`Self::lock`] refuses
/// every caller until the database is reopened.
pub struct BackendLock {
    backend: Mutex<Box<dyn Backend>>,
    gate: Mutex<Gate>,
    /// Signalled when a transaction closes.
    closed: Condvar,
}

/// Who holds the transaction gate, and whether the lock has failed closed.
#[derive(Default)]
struct Gate {
    owner: Option<ThreadId>,
    /// Why the backend needs recovery, once a transaction failed to settle.
    failed: Option<String>,
}

impl BackendLock {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        Self {
            backend: Mutex::new(backend),
            gate: Mutex::new(Gate::default()),
            closed: Condvar::new(),
        }
    }

    /// The backend, once no other thread holds a transaction open.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] once the lock has failed closed: an earlier
    /// transaction did not settle, and only reopening the database makes
    /// the store safe to read or write again.
    pub fn lock(&self) -> Result<BackendGuard<'_>> {
        let mut gate = self.gate.lock();
        self.wait_for_gate(&mut gate);
        if let Some(why) = &gate.failed {
            return Err(needs_recovery(why));
        }
        // Taken while still holding the gate, so no transaction can open
        // between the check and the lock.
        let guard = self.backend_guard();
        // A write can leave the backend unsettled before any commit: an
        // indeterminate commit of its own outside a transaction, or a
        // transaction SQLite ended underneath its owner. Fail closed then,
        // for the owner too, so nothing reads or writes past the loss.
        // Commit, rollback and drop settle through `backend_guard`.
        if guard.transaction_state() == TransactionState::NeedsRecovery {
            let why = match gate.owner {
                None => "a write outside a transaction did not settle",
                Some(_) => "the open transaction was lost",
            }
            .to_string();
            let refused = needs_recovery(&why);
            gate.failed = Some(why);
            return Err(refused);
        }
        Ok(guard)
    }

    /// The backend, without the gate checks: only for a thread that holds
    /// the gate.
    fn backend_guard(&self) -> BackendGuard<'_> {
        MutexGuard::map(self.backend.lock(), |b| &mut **b)
    }

    /// Wait until no thread but the caller holds the gate.
    fn wait_for_gate(&self, gate: &mut MutexGuard<'_, Gate>) {
        let me = std::thread::current().id();
        while gate.owner.is_some_and(|o| o != me) {
            self.closed.wait(gate);
        }
    }

    /// Whether the calling thread holds the gate: has a transaction open.
    pub(crate) fn held_here(&self) -> bool {
        self.gate.lock().owner == Some(std::thread::current().id())
    }

    /// Claim the gate for the calling thread. A thread that already holds
    /// it is refused, and keeps it: transactions do not nest, and a nested
    /// begin must not touch the outer transaction's ownership.
    fn claim(&self) -> Result<()> {
        let me = std::thread::current().id();
        let mut gate = self.gate.lock();
        if gate.owner == Some(me) {
            return Err(StoreError::Invalid(
                "a transaction is already open on this thread; transactions do not nest"
                    .to_string(),
            ));
        }
        self.wait_for_gate(&mut gate);
        if let Some(why) = &gate.failed {
            return Err(needs_recovery(why));
        }
        gate.owner = Some(me);
        Ok(())
    }

    /// Release the gate and wake every waiting thread. `failed` fails the
    /// lock closed first, so no waiting thread reads unsettled state.
    fn release(&self, failed: Option<String>) {
        let mut gate = self.gate.lock();
        if failed.is_some() {
            gate.failed = failed;
        }
        gate.owner = None;
        drop(gate);
        self.closed.notify_all();
    }
}

/// The refusal a failed-closed lock gives every caller.
fn needs_recovery(why: &str) -> StoreError {
    StoreError::Engine(format!(
        "a transaction did not settle ({why}); the store refuses reads and writes until it is \
         reopened, which recovers it"
    ))
}

/// Bring the backend to [`TransactionState::Closed`] after a commit or a
/// rollback whose error, if any, is `failure`: roll back a transaction
/// still open, then check. `None` when it is safe to let other threads in;
/// otherwise why it is not.
fn settle(backend: &mut dyn Backend, failure: Option<String>) -> Option<String> {
    let mut why = failure;
    if backend.transaction_state() == TransactionState::Open {
        if let Err(e) = backend.rollback_transaction() {
            why = Some(format!("rolling back failed: {e}"));
        }
    }
    match backend.transaction_state() {
        TransactionState::Closed => None,
        TransactionState::Open => Some(why.unwrap_or_else(|| "it is still open".to_string())),
        TransactionState::NeedsRecovery => {
            Some(why.unwrap_or_else(|| "the backend needs recovery".to_string()))
        }
    }
}

impl fmt::Debug for BackendLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendLock").finish_non_exhaustive()
    }
}

/// An open transaction on a shared backend: committed by
/// [`Transaction::commit`], rolled back when dropped uncommitted (an error
/// or a panic in the work it wraps). While it is open, every other thread
/// waits at the backend's lock.
pub(crate) struct Transaction<'t> {
    backend: &'t BackendLock,
    open: bool,
}

impl<'t> Transaction<'t> {
    pub(crate) fn begin(backend: &'t BackendLock) -> Result<Self> {
        backend.claim()?;
        let mut guard = backend.backend_guard();
        // A write outside any transaction may have left the backend
        // unsettled; `claim` sees only the gate, so check the backend too.
        if guard.transaction_state() == TransactionState::NeedsRecovery {
            drop(guard);
            let why = "a write outside a transaction did not settle".to_string();
            let refused = needs_recovery(&why);
            backend.release(Some(why));
            return Err(refused);
        }
        let begun = guard.begin_transaction();
        drop(guard);
        if let Err(e) = begun {
            backend.release(None);
            return Err(e);
        }
        Ok(Self {
            backend,
            open: true,
        })
    }

    /// Commit, then let other threads in. A failed commit that leaves the
    /// transaction open is rolled back first, and one that leaves it
    /// unsettled fails the lock closed (see [`Backend::commit_transaction`]),
    /// so the gate never opens on a half-settled batch. A commit that
    /// reports success but leaves the backend unsettled is an error too.
    pub(crate) fn commit(mut self) -> Result<()> {
        self.open = false;
        let (committed, failed) = {
            let mut backend = self.backend.backend_guard();
            let committed = backend.commit_transaction();
            let failed = settle(
                &mut *backend,
                committed.as_ref().err().map(ToString::to_string),
            );
            (committed, failed)
        };
        let unsettled = failed.as_deref().map(needs_recovery);
        self.backend.release(failed);
        committed.and(unsettled.map_or(Ok(()), Err))
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        let failed = {
            let mut backend = self.backend.backend_guard();
            let rolled_back = backend.rollback_transaction();
            settle(&mut *backend, rolled_back.err().map(|e| e.to_string()))
        };
        if let Some(why) = &failed {
            // Nothing to return it to: the lock now refuses every caller,
            // and reopening the database finishes the rollback.
            eprintln!("store: a transaction did not roll back cleanly: {why}");
        }
        self.backend.release(failed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::engine::EngineTables;

    fn engine() -> BackendLock {
        BackendLock::new(Box::new(EngineTables::open_temporary().unwrap()))
    }

    #[test]
    fn another_thread_waits_for_the_transaction_and_then_sees_it_whole() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let backend = engine();
        let tx = Transaction::begin(&backend).unwrap();
        VectorStore::put(&mut *backend.lock().unwrap(), "a", 0, &[1]).unwrap();
        let wrote = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let other = scope.spawn(|| {
                // Waits at the lock until the transaction commits, then
                // sees both of its writes.
                let seen = VectorStore::count(&*backend.lock().unwrap()).unwrap();
                VectorStore::put(&mut *backend.lock().unwrap(), "x", 0, &[1]).unwrap();
                wrote.store(true, Ordering::SeqCst);
                seen
            });
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(!wrote.load(Ordering::SeqCst), "the other thread waited");
            VectorStore::put(&mut *backend.lock().unwrap(), "b", 0, &[1]).unwrap();
            tx.commit().unwrap();
            assert_eq!(other.join().unwrap(), 2, "it saw the transaction whole");
        });
        assert_eq!(VectorStore::count(&*backend.lock().unwrap()).unwrap(), 3);
    }

    #[test]
    fn a_transaction_dropped_uncommitted_is_rolled_back() {
        let backend = engine();
        {
            let _tx = Transaction::begin(&backend).unwrap();
            VectorStore::put(&mut *backend.lock().unwrap(), "y", 0, &[1]).unwrap();
        }
        assert_eq!(VectorStore::count(&*backend.lock().unwrap()).unwrap(), 0);
        assert!(
            backend.lock().unwrap().begin_transaction().is_ok(),
            "the transaction is closed and the backend takes a new one"
        );
    }

    #[test]
    fn a_failed_begin_releases_the_gate() {
        let backend = engine();
        backend.lock().unwrap().begin_transaction().unwrap();
        // The backend already has one open, so this begin is refused...
        assert!(Transaction::begin(&backend).is_err());
        // ...and leaves the gate free for other threads.
        std::thread::scope(|scope| {
            scope
                .spawn(|| VectorStore::count(&*backend.lock().unwrap()).unwrap())
                .join()
                .unwrap();
        });
    }

    /// A begin on a thread that already holds a transaction is refused,
    /// and the outer transaction keeps the gate: no other thread gets in
    /// until it commits.
    #[test]
    fn a_nested_begin_is_refused_and_the_outer_keeps_the_gate() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let backend = engine();
        let outer = Transaction::begin(&backend).unwrap();
        VectorStore::put(&mut *backend.lock().unwrap(), "a", 0, &[1]).unwrap();
        let nested = Transaction::begin(&backend);
        assert!(
            matches!(&nested, Err(StoreError::Invalid(why)) if why.contains("do not nest")),
            "{:?}",
            nested.as_ref().err()
        );
        drop(nested);
        let got_in = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let other = scope.spawn(|| {
                let seen = VectorStore::count(&*backend.lock().unwrap()).unwrap();
                got_in.store(true, Ordering::SeqCst);
                seen
            });
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(!got_in.load(Ordering::SeqCst), "the gate stayed shut");
            VectorStore::put(&mut *backend.lock().unwrap(), "b", 0, &[1]).unwrap();
            outer.commit().unwrap();
            assert_eq!(other.join().unwrap(), 2, "it saw the outer batch whole");
        });
    }

    /// Readers on other threads, through the shared store wrapper, see a
    /// committed batch whole and a rolled-back one not at all, even when
    /// the writer tries a nested transaction part-way through.
    #[test]
    fn readers_see_a_whole_batch_or_none_of_a_rollback() {
        use crate::db::vectors::Vectors;
        use crate::db::{Database, Store};
        use std::sync::atomic::{AtomicBool, Ordering};
        const BATCH: usize = 5;
        let db = Database::open_in_memory().unwrap();
        let handle = db.backend_handle();
        let done = AtomicBool::new(false);
        let pause = || std::thread::sleep(std::time::Duration::from_millis(5));
        let batch = |page: &Store<'_>, prefix: &str| -> Result<()> {
            for i in 0..BATCH {
                Vectors::new(page).put(&format!("{prefix}{i}"), 0, &[1])?;
                if i == 2 {
                    let nested = page.transaction(|_| Ok::<_, StoreError>(()));
                    assert!(nested.is_err(), "a nested transaction is refused");
                }
                pause();
            }
            Ok(())
        };
        let seen: Vec<usize> = std::thread::scope(|scope| {
            let readers: Vec<_> = (0..3)
                .map(|_| {
                    let handle = handle.clone();
                    let done = &done;
                    scope.spawn(move || {
                        let store = Store::unlocked(handle, None);
                        let mut seen = Vec::new();
                        loop {
                            seen.push(
                                VectorStore::count(&*store.backend().lock().unwrap()).unwrap(),
                            );
                            if done.load(Ordering::SeqCst) {
                                return seen;
                            }
                            std::thread::yield_now();
                        }
                    })
                })
                .collect();
            let writer = Store::unlocked(handle.clone(), None);
            writer.transaction(|page| batch(page, "kept")).unwrap();
            let rolled_back: Result<()> = writer.transaction(|page| {
                batch(page, "dropped")?;
                Err(StoreError::Invalid("give up".into()))
            });
            assert!(rolled_back.is_err());
            done.store(true, Ordering::SeqCst);
            readers
                .into_iter()
                .flat_map(|r| r.join().unwrap())
                .collect()
        });
        assert!(!seen.is_empty());
        let partial: Vec<&usize> = seen.iter().filter(|n| **n != 0 && **n != BATCH).collect();
        assert!(
            partial.is_empty(),
            "a reader saw a partial batch: {partial:?}"
        );
        assert_eq!(VectorStore::count(&*handle.lock().unwrap()).unwrap(), BATCH);
    }

    /// A file database in a fresh directory, and the directory.
    fn disposable(name: &str) -> (crate::db::Database, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("remind_me_backend_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::Database::open(dir.join("memory.db")).unwrap();
        (db, dir)
    }

    /// What a store holds of a page that wrote memory `m` and a vector for
    /// it, or the error a failed-closed store gives.
    fn page_state(store: &crate::db::Store<'_>) -> Result<(bool, bool)> {
        use crate::db::memories::Memories;
        use crate::db::vectors::Vectors;
        Ok((
            Memories::new(store).exists("m")?,
            Vectors::new(store).any_embedding()?.is_some(),
        ))
    }

    /// A page writing memory `m` and a vector for it, arming `fault` last,
    /// so it hits the commit (or, for an error, the rollback) and nothing
    /// before it.
    fn write_page(
        store: &crate::db::Store<'_>,
        fault: rusty_multimodal_db_engine::test_support::Fault,
        give_up: bool,
    ) -> Result<()> {
        use crate::db::memories::{Memories, NewMemory};
        use crate::db::vectors::Vectors;
        store.transaction(|page| {
            Memories::new(page).insert(&NewMemory::new("m", "quokka", "2026-11-21T00:00:00Z"))?;
            Vectors::new(page).put("m", 0, &[1])?;
            rusty_multimodal_db_engine::test_support::arm_fault(fault);
            if give_up {
                return Err(StoreError::Invalid("give up".into()));
            }
            Ok(())
        })
    }

    /// Every caller, on this thread and another, is refused.
    fn refuses_everyone(db: &crate::db::Database) {
        let store = db.store();
        let refused = page_state(&store).unwrap_err().to_string();
        assert!(refused.contains("refuses reads and writes"), "{refused}");
        assert!(store.transaction(|_| Ok::<_, StoreError>(())).is_err());
        drop(store);
        std::thread::scope(|scope| {
            let other = scope.spawn(|| {
                let store = db.store();
                page_state(&store).is_err()
            });
            assert!(other.join().unwrap(), "another thread is refused too");
        });
    }

    /// The journal fails after writing the whole redo frame (its sync
    /// fails): the outcome is indeterminate, so the store fails closed and
    /// no one reads the unsettled page. Reopening replays the whole frame.
    #[test]
    fn an_indeterminate_commit_fails_closed_and_recovers_whole() {
        use rusty_multimodal_db_engine::test_support::Fault;
        let (db, dir) = disposable("indeterminate");
        assert!(write_page(&db.store(), Fault::JournalSync, false).is_err());
        refuses_everyone(&db);
        drop(db);
        let db = crate::db::Database::open(dir.join("memory.db")).unwrap();
        assert_eq!(page_state(&db.store()).unwrap(), (true, true));
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The journal fails part-way through the redo frame: the store fails
    /// closed, and reopening finds none of the page.
    #[test]
    fn a_torn_commit_fails_closed_and_recovers_none() {
        use rusty_multimodal_db_engine::test_support::Fault;
        let (db, dir) = disposable("torn");
        assert!(write_page(&db.store(), Fault::JournalWritePrefix(7), false).is_err());
        refuses_everyone(&db);
        drop(db);
        let db = crate::db::Database::open(dir.join("memory.db")).unwrap();
        assert_eq!(page_state(&db.store()).unwrap(), (false, false));
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A rollback whose cleanup fails (the undo log's checkpoint) fails the
    /// store closed on the drop path too; reopening finds none of the page.
    #[test]
    fn a_rollback_that_does_not_settle_fails_closed() {
        use rusty_multimodal_db_engine::test_support::Fault;
        let (db, dir) = disposable("rollback");
        let gave_up = write_page(&db.store(), Fault::CheckpointAfterRename, true);
        assert!(gave_up.unwrap_err().to_string().contains("give up"));
        refuses_everyone(&db);
        drop(db);
        let db = crate::db::Database::open(dir.join("memory.db")).unwrap();
        assert_eq!(page_state(&db.store()).unwrap(), (false, false));
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A database opened on a backend handed in from outside runs the
    /// usual start-up work and serves stores whose transactions and
    /// whole-store operations go through that backend.
    #[test]
    fn a_database_opens_on_a_given_backend() {
        use crate::db::memories::{Memories, NewMemory};
        use crate::db::{Database, StoreError};
        let db = Database::open_backend(Box::new(EngineTables::open_temporary().unwrap()), None)
            .unwrap();
        let store = db.store();
        let refused: Result<()> = store.transaction(|page| {
            Memories::new(page).insert(&NewMemory::new("m1", "quokka", "2026-11-21T00:00:00Z"))?;
            Err(StoreError::Invalid("give up".into()))
        });
        assert!(refused.is_err());
        assert!(!Memories::new(&store).exists("m1").unwrap(), "rolled back");
        store
            .transaction(|page| {
                Memories::new(page).insert(&NewMemory::new("m1", "quokka", "2026-11-21T00:00:00Z"))
            })
            .unwrap();
        assert!(Memories::new(&store).exists("m1").unwrap(), "committed");
        crate::db::derived::rebuild_indexes(&store).unwrap();
        drop(store);
        db.compact_store().unwrap();
    }
}
