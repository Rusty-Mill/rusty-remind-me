//! The engine-backed store (ADR-0023, phase 4), built one table group at a
//! time behind the `engine-store` feature.
//!
//! [`EngineTables`] holds the engine stores for the groups moved so far. A
//! [`super::Store`] carries it beside the SQLite connection, and each moved
//! repository reads and writes here instead of SQLite; every other group
//! stays on SQLite until its own step. Phase 5 makes the engine the default
//! and phase 6 removes SQLite.
//!
//! Ids follow the hub's layout (its ADR-0021): each record's engine id is a
//! UUID v5 of the node's string id, and the string stays on the record so a
//! collision is refused rather than merged.

pub(crate) mod analytics;
pub(crate) mod archives;
pub(crate) mod revisions;
pub(crate) mod saved_searches;
pub(crate) mod sync_log;
pub(crate) mod wiki;

use super::StoreError;
use rusty_multimodal_db_engine::dir_lock::DirLock;
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
/// background threads that open their own SQLite connections beside it.
pub type EngineHandle = std::sync::Arc<parking_lot::Mutex<EngineTables>>;

/// The environment variable that picks the backend for
/// [`super::Database::open_in_memory`]: `engine` or unset (SQLite).
pub const STORE_ENV: &str = "REMIND_ME_STORE";

/// The lock file inside the data directory, held while the tables are open.
const LOCK_FILE: &str = "node.lock";

/// The redo journal inside the data directory. Today it carries only the
/// id sequences (ADR-0023 §3c); cross-store batches (§3b) use it when the
/// groups that write together move.
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
    journal: Journal,
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
        let lock = DirLock::acquire(dir, LOCK_FILE).map_err(engine_error)?;
        let journal = open_journal(&dir.join(JOURNAL_FILE))?;
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
            journal,
            _lock: lock,
            _temporary: None,
        };
        // A table written by something other than this journal (the copy
        // tool, or an older build) must still never see an id reissued.
        let floor = analytics::max_id(&tables.snapshots);
        tables.journal.raise_to(analytics::SEQUENCE, floor);
        let floor = revisions::max_id(&tables.revisions);
        tables.journal.raise_to(revisions::SEQUENCE, floor);
        tables.wiki_search = wiki::index_pages(&tables.wiki_pages);
        Ok(tables)
    }

    /// The next id from the journal sequence `name`, durable before it is
    /// returned: a crash after this leaves a gap, never a reissued id.
    pub(crate) fn next_id(&mut self, name: &str) -> super::Result<i64> {
        let id = self.journal.allocate(name);
        self.journal
            .commit(&Batch::default())
            .map_err(engine_error)?;
        self.journal.checkpoint().map_err(engine_error)?;
        i64::try_from(id).map_err(|_| StoreError::Engine(format!("sequence {name} overflowed")))
    }

    /// Tables in a fresh directory that is removed when they are dropped:
    /// the engine's counterpart of an in-memory SQLite database.
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

/// Whether `REMIND_ME_STORE` asks for the engine.
pub(crate) fn engine_selected() -> bool {
    std::env::var(STORE_ENV).is_ok_and(|v| v.trim().eq_ignore_ascii_case("engine"))
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

/// Any engine failure, as a [`StoreError`].
pub(crate) fn engine_error(e: impl fmt::Display) -> StoreError {
    StoreError::Engine(e.to_string())
}

/// A delete's result, with an already-missing record counted as deleted.
pub(crate) fn deleted(result: std::result::Result<(), DeleteError<Uuid>>) -> super::Result<()> {
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

/// Open the journal at `path`. Every batch it could hand back to replay is
/// empty today, since no cross-store batch is written yet; one that is not
/// was written by a newer build, and is refused rather than dropped.
fn open_journal(path: &Path) -> super::Result<Journal> {
    let opened = Journal::open(path).map_err(engine_error)?;
    if opened.replay.iter().any(|batch| !batch.is_empty()) {
        return Err(StoreError::Engine(format!(
            "{} holds changes this build cannot apply",
            path.display()
        )));
    }
    Ok(opened.journal)
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
struct TemporaryDir(PathBuf);

impl TemporaryDir {
    fn fresh() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Self(dir)
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match EngineTables::open(dir) {
            Ok(tables) => return tables,
            Err(StoreError::Engine(why))
                if why.contains("in use by another process")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => panic!("reopening {}: {e}", dir.display()),
        }
    }
}

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
