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
use super::Result;
use parking_lot::{Condvar, MappedMutexGuard, Mutex, MutexGuard};
use std::any::Any;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::thread::ThreadId;

/// A whole store: every table group's port, and what spans them.
///
/// `Any` lets engine-only code (`crate::testing`'s raw-row helpers) find
/// the engine behind a handle and refuse plainly on any other backend.
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
    /// [`Self::commit_transaction`] or not at all.
    ///
    /// # Errors
    ///
    /// The backend's error if it refuses writes or one is already open.
    fn begin_transaction(&mut self) -> Result<()>;

    /// Make every write of the open transaction durable, as one.
    ///
    /// # Errors
    ///
    /// The backend's error if none is open, a write in it failed (the
    /// transaction is then rolled back), or it cannot be made durable.
    fn commit_transaction(&mut self) -> Result<()>;

    /// Put back everything the open transaction wrote.
    ///
    /// # Errors
    ///
    /// The backend's error if none is open or the rollback fails.
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
pub struct BackendLock {
    backend: Mutex<Box<dyn Backend>>,
    /// The thread holding a transaction open, if any.
    owner: Mutex<Option<ThreadId>>,
    /// Signalled when a transaction closes.
    closed: Condvar,
}

impl BackendLock {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        Self {
            backend: Mutex::new(backend),
            owner: Mutex::new(None),
            closed: Condvar::new(),
        }
    }

    /// The backend, once no other thread holds a transaction open.
    pub fn lock(&self) -> BackendGuard<'_> {
        let mut owner = self.owner.lock();
        self.wait_for_gate(&mut owner);
        // Taken while still holding the gate, so no transaction can open
        // between the check and the lock.
        MutexGuard::map(self.backend.lock(), |b| &mut **b)
    }

    /// Wait until no thread but the caller holds the gate.
    fn wait_for_gate(&self, owner: &mut MutexGuard<'_, Option<ThreadId>>) {
        let me = std::thread::current().id();
        while owner.is_some_and(|o| o != me) {
            self.closed.wait(owner);
        }
    }

    /// Claim the gate for the calling thread.
    fn claim(&self) {
        let mut owner = self.owner.lock();
        self.wait_for_gate(&mut owner);
        *owner = Some(std::thread::current().id());
    }

    /// Release the gate and wake every waiting thread.
    fn release(&self) {
        *self.owner.lock() = None;
        self.closed.notify_all();
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
        backend.claim();
        if let Err(e) = backend.lock().begin_transaction() {
            backend.release();
            return Err(e);
        }
        Ok(Self {
            backend,
            open: true,
        })
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        self.open = false;
        let committed = self.backend.lock().commit_transaction();
        self.backend.release();
        committed
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        if let Err(e) = self.backend.lock().rollback_transaction() {
            // Nothing to return it to: the backend now refuses writes, and
            // reopening it finishes the rollback.
            eprintln!("store: rolling back a transaction failed: {e}");
        }
        self.backend.release();
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
        VectorStore::put(&mut *backend.lock(), "a", 0, &[1]).unwrap();
        let wrote = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let other = scope.spawn(|| {
                // Waits at the lock until the transaction commits, then
                // sees both of its writes.
                let seen = VectorStore::count(&*backend.lock()).unwrap();
                VectorStore::put(&mut *backend.lock(), "x", 0, &[1]).unwrap();
                wrote.store(true, Ordering::SeqCst);
                seen
            });
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(!wrote.load(Ordering::SeqCst), "the other thread waited");
            VectorStore::put(&mut *backend.lock(), "b", 0, &[1]).unwrap();
            tx.commit().unwrap();
            assert_eq!(other.join().unwrap(), 2, "it saw the transaction whole");
        });
        assert_eq!(VectorStore::count(&*backend.lock()).unwrap(), 3);
    }

    #[test]
    fn a_transaction_dropped_uncommitted_is_rolled_back() {
        let backend = engine();
        {
            let _tx = Transaction::begin(&backend).unwrap();
            VectorStore::put(&mut *backend.lock(), "y", 0, &[1]).unwrap();
        }
        assert_eq!(VectorStore::count(&*backend.lock()).unwrap(), 0);
        assert!(
            backend.lock().begin_transaction().is_ok(),
            "the transaction is closed and the backend takes a new one"
        );
    }

    #[test]
    fn a_failed_begin_releases_the_gate() {
        let backend = engine();
        backend.lock().begin_transaction().unwrap();
        // The backend already has one open, so this begin is refused...
        assert!(Transaction::begin(&backend).is_err());
        // ...and leaves the gate free for other threads.
        std::thread::scope(|scope| {
            scope
                .spawn(|| VectorStore::count(&*backend.lock()).unwrap())
                .join()
                .unwrap();
        });
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
