//! The engine-backed store (ADR-0023), the node's only store since phase 6
//! (ADR-0025).
//!
//! [`EngineTables`] holds the engine stores for every table group. A
//! [`super::Store`] carries them, and each repository under `db` is a thin
//! layer over the module here that owns its group. The groups were moved
//! one at a time beside the SQLite store (phase 4), proven against it, and
//! the SQLite store then removed.
//!
//! Ids follow the hub's layout (its ADR-0021): each record's engine id is a
//! UUID v5 of the node's string id, and the string stays on the record so a
//! collision is refused rather than merged.

pub(crate) mod analytics;
pub(crate) mod archives;
pub mod copy;
pub(crate) mod core;
pub(crate) mod curation;
pub(crate) mod feedback;
pub(crate) mod graph;
pub(crate) mod imports;
pub(crate) mod memories;
pub(crate) mod memories_v31;
pub(crate) mod outbox;
pub(crate) mod page;
pub(crate) mod promotions;
pub(crate) mod references;
pub(crate) mod related;
pub(crate) mod reminders;
pub(crate) mod revisions;
pub(crate) mod saved_searches;
pub(crate) mod sessions;
pub(crate) mod stats;
pub(crate) mod sync_log;
pub(crate) mod testing;
pub(crate) mod vectors;
pub(crate) mod wiki;

use super::StoreError;
use rusty_multimodal_db_engine::dir_lock::{DirLock, DirLockError};
use rusty_multimodal_db_engine::durability::DurabilityError;
use rusty_multimodal_db_engine::generic::mmap_field::MmapFieldValue;
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::{DeleteError, GenericMmapStore};
use rusty_multimodal_db_engine::journal::{Batch, Journal};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

/// The engine tables, shared by the database, its stores, and the
/// background threads that work beside it.
pub type EngineHandle = std::sync::Arc<EngineLock>;

/// The engine tables behind their lock, shared by every thread that uses
/// them, and the page gate in front of it (see [`page`]).
///
/// While one thread holds a page open, every other thread's [`Self::lock`]
/// waits until the page is finished or abandoned, as a second SQLite writer
/// waits for a write transaction: its reads and writes then see the page
/// whole or not at all. The page's own thread passes straight through.
pub struct EngineLock {
    tables: parking_lot::Mutex<EngineTables>,
    /// The thread holding a page open, if any.
    page_owner: parking_lot::Mutex<Option<std::thread::ThreadId>>,
    /// Signalled when a page closes.
    page_closed: parking_lot::Condvar,
}

impl EngineLock {
    pub fn new(tables: EngineTables) -> Self {
        Self {
            tables: parking_lot::Mutex::new(tables),
            page_owner: parking_lot::Mutex::new(None),
            page_closed: parking_lot::Condvar::new(),
        }
    }

    /// The tables, once no other thread holds a page open.
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, EngineTables> {
        let me = std::thread::current().id();
        let mut owner = self.page_owner.lock();
        while owner.is_some_and(|o| o != me) {
            self.page_closed.wait(&mut owner);
        }
        // Taken while still holding the gate, so no page can open between
        // the check and the lock.
        self.tables.lock()
    }

    /// Claim the gate for the calling thread, once no other thread holds
    /// it.
    fn claim_page(&self) {
        let me = std::thread::current().id();
        let mut owner = self.page_owner.lock();
        while owner.is_some_and(|o| o != me) {
            self.page_closed.wait(&mut owner);
        }
        *owner = Some(me);
    }

    /// Release the gate and wake every waiting thread.
    fn release_page(&self) {
        *self.page_owner.lock() = None;
        self.page_closed.notify_all();
    }
}

impl fmt::Debug for EngineLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineLock").finish_non_exhaustive()
    }
}

/// The lock file inside the data directory, held while the tables are open.
const LOCK_FILE: &str = "node.lock";

/// The redo journal inside the data directory: the id sequences (ADR-0023
/// §3c), and the memories core's cross-store batches (§3b).
const JOURNAL_FILE: &str = "node.journal";

/// The namespace every node engine id is derived in. Pinned: changing it
/// would orphan every record already written.
const NODE_NAMESPACE: Uuid = Uuid::from_u128(0x7272_6d6e_6f64_4000_8000_0000_0000_0023);

/// The engine stores for the table groups moved so far.
///
/// Fields drop in declaration order: the stores unmap first, then the lock
/// is released, then a temporary directory is removed.
pub struct EngineTables {
    pub(crate) saved_searches: saved_searches::SavedSearchTable,
    pub(crate) seen: saved_searches::SeenTable,
    pub(crate) archives: archives::ArchiveTable,
    pub(crate) spans: archives::SpanTable,
    pub(crate) sync_log: sync_log::SyncLogTable,
    pub(crate) snapshots: analytics::SnapshotTable,
    pub(crate) revisions: revisions::RevisionTable,
    pub(crate) wiki_pages: wiki::PageTable,
    pub(crate) wiki_links: wiki::LinkTable,
    pub(crate) wiki_meta: wiki::MetaTable,
    /// Derived from `wiki_pages` at open, and kept in step by every page
    /// write; never stored.
    pub(crate) wiki_search: wiki::PageSearch,
    /// The memories core (see [`core`]).
    pub(crate) core: core::CoreTables,
    journal: Journal,
    /// The undo log of the open page, if any (see [`page`]).
    undo: Journal,
    /// The page open on the core, if any.
    page: Option<page::OpenPage>,
    /// Set when a batch was durable but did not reach every store: the
    /// tables then refuse writes until reopened, which applies it again.
    failed: bool,
    /// The directory the tables live in, for [`EngineTables::copy_files_to`].
    dir: PathBuf,
    _lock: DirLock,
    _temporary: Option<TemporaryDir>,
}

impl EngineTables {
    /// Open the tables in `dir`, creating any that do not exist yet.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if another process holds `dir`, or a store in
    /// it cannot be opened.
    pub fn open(dir: &Path) -> super::Result<Self> {
        let lock = DirLock::acquire(dir, LOCK_FILE).map_err(lock_error)?;
        let opened = Journal::open(&dir.join(JOURNAL_FILE)).map_err(engine_error)?;
        let undo = Journal::open(&dir.join(page::UNDO_FILE)).map_err(engine_error)?;
        let core = core::CoreTables::open(dir)?;
        let mut tables = Self {
            saved_searches: open_core(&dir.join("saved_searches.mmap"))?,
            seen: open_core(&dir.join("saved_search_seen.mmap"))?,
            archives: open_core(&dir.join("import_archives.mmap"))?,
            spans: open_core(&dir.join("import_archive_spans.mmap"))?,
            sync_log: open_core(&dir.join("sync_log.mmap"))?,
            snapshots: open_core(&dir.join("analytics_snapshots.mmap"))?,
            revisions: open_core(&dir.join("memory_revisions.mmap"))?,
            wiki_pages: open_core(&dir.join("wiki_pages.mmap"))?,
            wiki_links: open_core(&dir.join("wiki_links.mmap"))?,
            wiki_meta: open_core(&dir.join("wiki_meta.mmap"))?,
            wiki_search: wiki::PageSearch::new(),
            core,
            journal: opened.journal,
            undo: undo.journal,
            page: None,
            failed: false,
            dir: dir.to_path_buf(),
            _lock: lock,
            _temporary: None,
        };
        // An unfinished page first, then the batches after it.
        tables.roll_back(undo.replay)?;
        tables.replay(opened.replay)?;
        // A table written by something other than this journal (the copy
        // tool, or an older build) must still never see an id reissued.
        let floor = analytics::max_id(&tables.snapshots);
        tables.journal.raise_to(analytics::SEQUENCE, floor);
        let floor = revisions::max_id(&tables.revisions);
        tables.journal.raise_to(revisions::SEQUENCE, floor);
        let floor = outbox::max_id(&tables.core.outbox);
        tables.journal.raise_to(outbox::SEQUENCE, floor);
        tables.wiki_search = wiki::index_pages(&tables.wiki_pages);
        Ok(tables)
    }

    /// The next id from the journal sequence `name`, durable before it is
    /// returned: a crash after this leaves a gap, never a reissued id.
    pub(crate) fn next_id(&mut self, name: &str) -> super::Result<i64> {
        self.ensure_writable()?;
        let id = self.allocate(name)?;
        self.journal
            .commit(&Batch::default())
            .map_err(engine_error)?;
        self.journal.checkpoint().map_err(engine_error)?;
        Ok(id)
    }

    /// The next id from the journal sequence `name`, durable with the next
    /// commit: for a record committed in the same batch.
    pub(crate) fn allocate(&mut self, name: &str) -> super::Result<i64> {
        let id = self.journal.allocate(name);
        i64::try_from(id).map_err(|_| StoreError::Engine(format!("sequence {name} overflowed")))
    }

    /// Refuse writes after a batch failed part-way (see `failed`).
    fn ensure_writable(&self) -> super::Result<()> {
        if self.failed {
            return Err(StoreError::Engine(
                "a write failed part-way; reopen the tables to recover it".to_string(),
            ));
        }
        Ok(())
    }

    /// Compact every table whose insert log or slot file holds something
    /// to reclaim, and leave the rest alone: a compaction rewrites a
    /// table's files whole, so running one on every table on a timer
    /// would rewrite the store each time. How many tables were compacted.
    ///
    /// Opening a table already folds its log into a fresh blob, so a store
    /// that restarts often gains little; this bounds what a long-running
    /// daemon's tables hold between restarts (the hub does the same).
    /// Skipped while a page is open or after a failed write, when the
    /// stores may not agree with the journal yet.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] if a table cannot be rewritten. Compaction is
    /// crash-safe step by step, so an error leaves every table readable.
    pub(crate) fn compact_needed(&mut self) -> super::Result<usize> {
        if self.page.is_some() || self.failed {
            return Ok(0);
        }
        let mut compacted = 0;
        macro_rules! compact_each {
            ($($table:expr),+ $(,)?) => {$(
                if $table.needs_compaction() {
                    $table.compact().map_err(engine_error)?;
                    compacted += 1;
                }
            )+};
        }
        let core = &mut self.core;
        compact_each!(
            self.saved_searches,
            self.seen,
            self.archives,
            self.spans,
            self.sync_log,
            self.snapshots,
            self.revisions,
            self.wiki_pages,
            self.wiki_links,
            self.wiki_meta,
            core.memories,
            core.outbox,
            core.sends,
            core.flags,
            core.deliveries,
            core.feedback,
            core.entities,
            core.links,
            core.relations,
            core.associations,
            core.promotions,
            core.chunks,
            core.embedding_meta,
            core.chat_imports,
            core.dbs_imports,
            core.mempalace_imports,
            core.references,
            core.sessions,
        );
        Ok(compacted)
    }

    /// Copy every file of the tables into `dest`, which must not exist: a
    /// backup (`crate::backup`). The caller holds the tables, so no write lands mid-copy and
    /// no page is open on another thread; the copy opens as the tables
    /// would after a clean shutdown. The directory lock is not copied.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] naming the file that could not be copied.
    pub(crate) fn copy_files_to(&self, dest: &Path) -> super::Result<()> {
        let io =
            |path: &Path, e: std::io::Error| StoreError::Engine(format!("{}: {e}", path.display()));
        std::fs::create_dir(dest).map_err(|e| io(dest, e))?;
        for entry in std::fs::read_dir(&self.dir).map_err(|e| io(&self.dir, e))? {
            let entry = entry.map_err(|e| io(&self.dir, e))?;
            let from = entry.path();
            if entry.file_name() == LOCK_FILE || !from.is_file() {
                continue;
            }
            let to = dest.join(entry.file_name());
            std::fs::copy(&from, &to).map_err(|e| io(&from, e))?;
        }
        Ok(())
    }

    /// The bytes of every file in the directory the tables live in, the
    /// lock file included: what `storage_info` reports as the store's size.
    ///
    /// # Errors
    ///
    /// [`StoreError::Engine`] naming the entry that could not be read.
    pub(crate) fn size_on_disk(&self) -> super::Result<u64> {
        let io =
            |path: &Path, e: std::io::Error| StoreError::Engine(format!("{}: {e}", path.display()));
        let mut total = 0;
        for entry in std::fs::read_dir(&self.dir).map_err(|e| io(&self.dir, e))? {
            let entry = entry.map_err(|e| io(&self.dir, e))?;
            let meta = entry.metadata().map_err(|e| io(&entry.path(), e))?;
            if meta.is_file() {
                total += meta.len();
            }
        }
        Ok(total)
    }

    /// Tables in a fresh directory that is removed when they are dropped:
    /// an in-memory database.
    pub fn open_temporary() -> super::Result<Self> {
        let dir = TemporaryDir::fresh();
        let mut tables = Self::open(&dir.0)?;
        tables._temporary = Some(dir);
        Ok(tables)
    }
}

impl fmt::Debug for EngineTables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineTables").finish_non_exhaustive()
    }
}

/// The memories core of `tables`. It cannot fail since the switch-on (core
/// PR 5b); the `Result` stays until the repositories stop threading it.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn core_ref(tables: &EngineTables) -> super::Result<&core::CoreTables> {
    Ok(&tables.core)
}

/// [`core_ref`], for a write.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn core_mut(tables: &mut EngineTables) -> super::Result<&mut core::CoreTables> {
    Ok(&mut tables.core)
}

/// The error for a directory lock that could not be taken. When another
/// process holds it, name the usual holder and how to release it: on the
/// engine a node has one opener, and a client that could not use the
/// daemon lands here.
fn lock_error(e: DirLockError) -> StoreError {
    match e {
        DirLockError::Held(_) => StoreError::Engine(format!(
            "{e}. Another rusty-remind-me process holds this node's store, usually the \
             store daemon; `rusty-remind-me daemon stop` releases it"
        )),
        other => engine_error(other),
    }
}

/// The engine id for the node's string id `id`.
pub(crate) fn engine_id(id: &str) -> Uuid {
    Uuid::new_v5(&NODE_NAMESPACE, id.as_bytes())
}

/// The engine id for a pair of string ids. Length-prefixed, so no two pairs
/// spell the same key.
pub(crate) fn pair_engine_id(first: &str, second: &str) -> Uuid {
    engine_id(&format!("{}:{first}|{second}", first.len()))
}

/// Refuse to treat two different string ids as one record: `stored` is the
/// id on the record already at `incoming`'s engine id.
pub(crate) fn ensure_same_id(stored: &str, incoming: &str) -> super::Result<()> {
    if stored == incoming {
        return Ok(());
    }
    Err(StoreError::Engine(format!(
        "id {incoming:?} maps to the same engine id as the stored {stored:?}; refusing to merge"
    )))
}

/// The engine's words for a table written under another record layout.
const TAG_MISMATCH: &str = "schema tag mismatch";

/// Any engine failure, as a [`StoreError`].
///
/// A table whose record layout this build does not know is not an engine
/// fault: the store was written by another build of this program. It is
/// told apart, and refused with a message that says what to do, because the
/// engine's own words are a hash comparison. The engine reports it as text,
/// so it is recognised by that text; `a_store_written_under_another_layout_
/// is_refused_in_plain_words` provokes the real error to keep this honest.
pub(crate) fn engine_error(e: impl fmt::Display) -> StoreError {
    let detail = e.to_string();
    if detail.contains(TAG_MISMATCH) {
        return StoreError::Invalid(format!(
            "this store was written by a different version of rusty-remind-me (this build is {}) \
             and its record layout is not one this build reads. The store is unchanged. Open it \
             with the version that wrote it, or restore a backup from before the upgrade. \
             Engine detail: {detail}",
            env!("CARGO_PKG_VERSION")
        ));
    }
    StoreError::Engine(detail)
}

/// A delete's result, with an already-missing record counted as deleted.
pub(crate) fn deleted<I>(result: std::result::Result<(), DeleteError<I>>) -> super::Result<()>
where
    DeleteError<I>: fmt::Display,
{
    match result {
        Ok(()) | Err(DeleteError::NotFound(_)) => Ok(()),
        Err(e) => Err(engine_error(e)),
    }
}

/// `timestamp` in µs since the epoch, or 0 if it is not RFC 3339. Only an
/// engine scan slot holds it; records keep the text as given.
pub(crate) fn micros(timestamp: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|t| t.timestamp_micros())
        .unwrap_or(0)
}

/// Open the engine store at `path`, or create an empty one there.
fn open_core<R, Index, Slot>(path: &Path) -> super::Result<GenericMmapStore<R, Index, Slot>>
where
    R: Record
        + IndexedField<Index>
        + ScannableField<Slot>
        + Clone
        + Serialize
        + DeserializeOwned
        + SchemaTag,
    R::Id: MmapFieldValue + Serialize + DeserializeOwned,
    R::ScanValue: MmapFieldValue,
{
    let opened: Result<_, DurabilityError> = if path.exists() {
        GenericMmapStore::open_portable(path)
    } else {
        GenericMmapStore::create(Vec::new(), path)
    };
    opened.map_err(engine_error)
}

/// A directory under the system temp dir, removed on drop.
pub(crate) struct TemporaryDir(PathBuf);

impl TemporaryDir {
    pub(crate) fn fresh() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Self(dir)
    }
}

impl TemporaryDir {
    /// The directory, which does not exist until something creates it.
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDir {
    fn drop(&mut self) {
        // Best effort: a leftover temp dir is harmless, and a panic in drop
        // would hide whatever failure is already unwinding.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Open the tables in `dir` again right after a test dropped them.
///
/// The directory lock is an `flock`, which lasts while any copy of its file
/// descriptor is open, and a child another test thread forks holds a copy
/// until it execs. So the release can take a moment to show: retry while the
/// lock reads as held, and fail at once on anything else.
#[cfg(test)]
pub(crate) fn reopen(dir: &Path) -> EngineTables {
    retry_while_locked(|| EngineTables::open(dir))
        .unwrap_or_else(|e| panic!("reopening {}: {e}", dir.display()))
}

/// Run `open` until it stops failing on a held lock, for up to five
/// seconds (see [`reopen`]), and return what it last returned.
#[cfg(test)]
pub(crate) fn retry_while_locked<T>(open: impl Fn() -> super::Result<T>) -> super::Result<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match open() {
            Err(StoreError::Engine(why))
                if why.contains("in use by another process")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            opened => return opened,
        }
    }
}

#[cfg(test)]
mod open_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_ids_are_stable() {
        // The on-disk identity of every record: pinned so a change to the
        // namespace or the hashing shows up here, not as an empty store.
        assert_eq!(
            engine_id("s1").to_string(),
            Uuid::new_v5(&NODE_NAMESPACE, b"s1").to_string()
        );
        assert_ne!(pair_engine_id("a|b", "c"), pair_engine_id("a", "b|c"));
    }

    #[test]
    fn a_collision_is_refused_not_merged() {
        assert!(ensure_same_id("s1", "s1").is_ok());
        assert!(matches!(
            ensure_same_id("s1", "s2"),
            Err(StoreError::Engine(_))
        ));
    }

    #[test]
    fn a_second_open_of_one_directory_is_refused() {
        let first = EngineTables::open_temporary().unwrap();
        let dir = first._temporary.as_ref().unwrap().0.clone();
        assert!(matches!(
            EngineTables::open(&dir),
            Err(StoreError::Engine(_))
        ));
    }

    #[test]
    fn a_journal_with_changes_this_build_cannot_apply_is_refused() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_foreign_journal_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut journal = Journal::open(&dir.join(JOURNAL_FILE)).unwrap().journal;
            let mut batch = Batch::default();
            batch.put("from_a_newer_build", vec![1], vec![2]);
            journal.commit(&batch).unwrap();
        }
        let refused = EngineTables::open(&dir);
        assert!(
            matches!(&refused, Err(StoreError::Engine(why)) if why.contains("cannot apply")),
            "{refused:?}"
        );
        drop(refused);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_temporary_directory_is_removed_on_drop() {
        let tables = EngineTables::open_temporary().unwrap();
        let dir = tables._temporary.as_ref().unwrap().0.clone();
        assert!(dir.exists());
        drop(tables);
        assert!(!dir.exists());
    }
}
