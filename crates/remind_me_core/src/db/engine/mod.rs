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

pub(crate) mod saved_searches;

use super::StoreError;
use rusty_multimodal_db_engine::dir_lock::DirLock;
use rusty_multimodal_db_engine::durability::DurabilityError;
use rusty_multimodal_db_engine::generic::mmap_field::MmapFieldValue;
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

/// The environment variable that picks the backend for
/// [`super::Database::open_in_memory`]: `engine` or unset (SQLite).
pub const STORE_ENV: &str = "REMIND_ME_STORE";

/// The lock file inside the data directory, held while the tables are open.
const LOCK_FILE: &str = "node.lock";

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
        Ok(Self {
            saved_searches: open_core(&dir.join("saved_searches.mmap"))?,
            seen: open_core(&dir.join("saved_search_seen.mmap"))?,
            _lock: lock,
            _temporary: None,
        })
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

/// Open the engine store at `path`, or create an empty one there.
fn open_core<R, Index, Slot>(path: &Path) -> super::Result<GenericMmapStore<R, Index, Slot>>
where
    R: Record<Id = Uuid>
        + IndexedField<Index>
        + ScannableField<Slot>
        + Clone
        + Serialize
        + DeserializeOwned
        + SchemaTag,
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
    fn a_temporary_directory_is_removed_on_drop() {
        let tables = EngineTables::open_temporary().unwrap();
        let dir = tables._temporary.as_ref().unwrap().0.clone();
        assert!(dir.exists());
        drop(tables);
        assert!(!dir.exists());
    }
}
