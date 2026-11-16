//! Storage for per-memory edit history: `memory_revisions`, and the tracked
//! columns of `memories` a revision snapshots and a revert writes back.
//!
//! [`HistoryStore`] is the port (ADR-0035); the engine's adapter is in
//! `db::engine::revisions`. [`Revisions`] is what callers take: it locks the
//! store's tables for each call and goes through the port. The rules
//! stay in [`crate::history`]: which columns are tracked, what counts as a
//! change, and that a revert is itself a revisioned edit.

use super::engine::EngineLock;
use super::{Result, Store};
use crate::models::MemoryRevision;

/// The columns a revision snapshots, in their stored form: tags and metadata
/// as JSON strings, so comparing them with an update is like for like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracked {
    pub content: String,
    pub category: String,
    pub tags: String,
    pub metadata: String,
    /// `None` when unreadable, as in a revision captured before the column
    /// existed.
    pub sensitive: Option<bool>,
    /// `Some(project)`, which may itself be `None` (no project), or `None`
    /// when unknown: a revision captured before schema v33 did not record it.
    pub project: Option<Option<String>>,
}

/// The revision log and the tracked memory columns, as a backend stores
/// them (ADR-0035). [`conformance`] holds the behaviour every
/// implementation must show.
pub trait HistoryStore {
    /// Whether `memory_id` names a memory that is not deleted.
    fn is_live(&self, memory_id: &str) -> Result<bool>;

    /// `memory_id`'s tracked columns as stored now, deleted or not, or
    /// `None` if there is no such memory.
    fn current(&self, memory_id: &str) -> Result<Option<Tracked>>;

    /// Remove every revision of `memory_id`. None is not an error.
    fn delete_for(&mut self, memory_id: &str) -> Result<()>;

    /// Remove the revisions of every tombstoned memory. How many went.
    fn delete_of_tombstones(&mut self) -> Result<usize>;

    /// Append a revision of `memory_id` holding `values`, edited at
    /// `edited_at`, under the next revision id.
    fn insert(
        &mut self,
        memory_id: &str,
        values: &Tracked,
        edited_at: &str,
        reason: Option<&str>,
    ) -> Result<()>;

    /// `memory_id`'s revisions, newest `edited_at` first (compared as text),
    /// then highest id first, at most `limit`.
    fn list(&self, memory_id: &str, limit: usize) -> Result<Vec<MemoryRevision>>;

    /// The tracked values revision `revision_id` holds, if it belongs to
    /// `memory_id`.
    fn revision(&self, memory_id: &str, revision_id: i64) -> Result<Option<Tracked>>;

    /// Write `values` into `memory_id`'s tracked columns, stamping
    /// `updated_at`. A `None` `sensitive` is written as not sensitive, and a
    /// `None` `project` leaves the project as it is.
    fn restore(&mut self, memory_id: &str, values: &Tracked, updated_at: &str) -> Result<()>;
}

/// The revision log and tracked memory columns of a [`Store`], one locked
/// call at a time.
pub struct Revisions<'c> {
    engine: &'c EngineLock,
}

impl<'c> Revisions<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            engine: store.engine(),
        }
    }

    /// See [`HistoryStore::is_live`].
    pub fn is_live(&self, memory_id: &str) -> Result<bool> {
        HistoryStore::is_live(&*self.engine.lock(), memory_id)
    }

    /// See [`HistoryStore::current`].
    pub fn current(&self, memory_id: &str) -> Result<Option<Tracked>> {
        HistoryStore::current(&*self.engine.lock(), memory_id)
    }

    /// See [`HistoryStore::delete_for`]: a deleted memory's text must not
    /// stay readable in its history (ADR-0024).
    pub fn delete_for(&self, memory_id: &str) -> Result<()> {
        HistoryStore::delete_for(&mut *self.engine.lock(), memory_id)
    }

    /// See [`HistoryStore::delete_of_tombstones`]: for the memories deleted
    /// before a delete took their revisions with it.
    pub fn delete_of_tombstones(&self) -> Result<usize> {
        HistoryStore::delete_of_tombstones(&mut *self.engine.lock())
    }

    /// See [`HistoryStore::insert`].
    pub fn insert(
        &self,
        memory_id: &str,
        values: &Tracked,
        edited_at: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        HistoryStore::insert(
            &mut *self.engine.lock(),
            memory_id,
            values,
            edited_at,
            reason,
        )
    }

    /// See [`HistoryStore::list`]: the id breaks ties, so revisions captured
    /// within the same clock tick still list in the order they were written.
    pub fn list(&self, memory_id: &str, limit: usize) -> Result<Vec<MemoryRevision>> {
        HistoryStore::list(&*self.engine.lock(), memory_id, limit)
    }

    /// See [`HistoryStore::revision`].
    pub fn revision(&self, memory_id: &str, revision_id: i64) -> Result<Option<Tracked>> {
        HistoryStore::revision(&*self.engine.lock(), memory_id, revision_id)
    }

    /// See [`HistoryStore::restore`].
    pub fn restore(&self, memory_id: &str, values: &Tracked, updated_at: &str) -> Result<()> {
        HistoryStore::restore(&mut *self.engine.lock(), memory_id, values, updated_at)
    }
}

/// The behaviour every [`HistoryStore`] must show, run against each backend
/// by [`history_conformance`] (ADR-0035).
///
/// Revisions hang off memories, so a backend under test implements
/// [`MemoryStore`](crate::db::memories::MemoryStore) too, and the checks that
/// need a memory seed it through that port.
#[cfg(test)]
pub(crate) mod conformance {
    use super::{HistoryStore, Tracked};
    use crate::db::memories::{MemoryStore, NewMemory};

    pub(crate) const NOW: &str = "2026-09-27T12:00:00+00:00";

    /// Add a live memory `id` with content `"x"`.
    fn add<S: MemoryStore>(store: &mut S, id: &str) {
        MemoryStore::insert(store, &NewMemory::new(id, "x", NOW)).unwrap();
    }

    /// Tombstone memory `id`.
    fn delete<S: MemoryStore>(store: &mut S, id: &str) {
        MemoryStore::delete_live(store, id, Some(NOW)).unwrap();
    }

    fn values(content: &str) -> Tracked {
        Tracked {
            content: content.to_string(),
            category: "fact".to_string(),
            tags: "[]".to_string(),
            metadata: "{}".to_string(),
            sensitive: Some(true),
            project: None,
        }
    }

    pub(crate) fn revisions_list_newest_first_and_belong_to_their_memory<S: HistoryStore>(
        store: &mut S,
    ) {
        let same_tick = "2026-09-27T00:00:01+00:00";
        store
            .insert("m1", &values("a"), "2026-09-27T00:00:00+00:00", None)
            .unwrap();
        store.insert("m1", &values("b"), same_tick, None).unwrap();
        store
            .insert("m1", &values("c"), same_tick, Some("revert"))
            .unwrap();
        store
            .insert("m2", &values("other"), same_tick, None)
            .unwrap();

        let listed = store.list("m1", 10).unwrap();
        let contents: Vec<&str> = listed.iter().map(|r| r.content.as_str()).collect();
        assert_eq!(contents, ["c", "b", "a"], "same tick: last written first");
        assert_eq!(listed[0].revision_reason.as_deref(), Some("revert"));
        assert_eq!(listed[0].sensitive, Some(true));
        assert_eq!(store.list("m1", 2).unwrap().len(), 2);

        let oldest = listed[2].id;
        assert_eq!(store.revision("m1", oldest).unwrap(), Some(values("a")));
        assert_eq!(
            store.revision("m2", oldest).unwrap(),
            None,
            "a revision id from another memory is not found"
        );
    }

    pub(crate) fn delete_for_takes_only_that_memorys_revisions<S: HistoryStore>(store: &mut S) {
        store.insert("m1", &values("a"), NOW, None).unwrap();
        store.insert("m2", &values("b"), NOW, None).unwrap();
        store.delete_for("m1").unwrap();
        store.delete_for("missing").unwrap();
        assert!(store.list("m1", 10).unwrap().is_empty());
        assert_eq!(store.list("m2", 10).unwrap().len(), 1);
    }

    pub(crate) fn tombstones_lose_their_revisions<S: HistoryStore + MemoryStore>(store: &mut S) {
        add(store, "kept");
        add(store, "gone");
        for id in ["kept", "gone", "gone"] {
            HistoryStore::insert(store, id, &values("v"), NOW, None).unwrap();
        }
        delete(store, "gone");
        assert!(store.is_live("kept").unwrap());
        assert!(!store.is_live("gone").unwrap());
        assert!(!store.is_live("missing").unwrap());
        assert_eq!(
            store.current("gone").unwrap().map(|t| t.content),
            Some(crate::sync::TOMBSTONE_CONTENT.to_string()),
            "a tombstone's text is gone, but the row still reads"
        );
        assert_eq!(store.delete_of_tombstones().unwrap(), 2);
        assert_eq!(store.delete_of_tombstones().unwrap(), 0);
        assert_eq!(store.list("kept", 10).unwrap().len(), 1);
    }

    pub(crate) fn restore_writes_the_tracked_columns_back<S: HistoryStore + MemoryStore>(
        store: &mut S,
    ) {
        add(store, "m");
        assert!(store.current("missing").unwrap().is_none());
        let before = store.current("m").unwrap().unwrap();
        assert_eq!(before.content, "x");

        let mut back = values("restored");
        back.project = Some(Some("p".to_string()));
        store.restore("m", &back, NOW).unwrap();
        assert_eq!(store.current("m").unwrap(), Some(back.clone()));

        // `None` sensitive reads back as not sensitive; `None` project keeps it.
        let unknown = Tracked {
            tags: "not json".to_string(),
            sensitive: None,
            project: None,
            ..values("again")
        };
        store.restore("m", &unknown, NOW).unwrap();
        let now = store.current("m").unwrap().unwrap();
        assert_eq!(now.sensitive, Some(false));
        assert_eq!(now.project, Some(Some("p".to_string())));
        assert_eq!(now.content, "again");
        assert_eq!(now.tags, "not json", "stored as given, not parsed");
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`.
#[cfg(test)]
macro_rules! history_conformance {
    ($make:expr) => {
        use $crate::db::history::conformance;

        #[test]
        fn revisions_list_newest_first_and_belong_to_their_memory() {
            conformance::revisions_list_newest_first_and_belong_to_their_memory(&mut $make);
        }
        #[test]
        fn delete_for_takes_only_that_memorys_revisions() {
            conformance::delete_for_takes_only_that_memorys_revisions(&mut $make);
        }
        #[test]
        fn tombstones_lose_their_revisions() {
            conformance::tombstones_lose_their_revisions(&mut $make);
        }
        #[test]
        fn restore_writes_the_tracked_columns_back() {
            conformance::restore_writes_the_tracked_columns_back(&mut $make);
        }
    };
}
#[cfg(test)]
pub(crate) use history_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    #[test]
    fn the_repository_reaches_the_store_through_the_port() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        Memories::new(&store)
            .insert(&NewMemory::new("m", "x", conformance::NOW))
            .unwrap();
        let revisions = Revisions::new(&store);
        let current = revisions.current("m").unwrap().unwrap();
        revisions
            .insert("m", &current, conformance::NOW, None)
            .unwrap();
        let listed = revisions.list("m", 10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            revisions.revision("m", listed[0].id).unwrap(),
            Some(current.clone())
        );
        revisions.restore("m", &current, conformance::NOW).unwrap();
        assert!(revisions.is_live("m").unwrap());
        revisions.delete_for("m").unwrap();
        assert_eq!(revisions.delete_of_tombstones().unwrap(), 0);
        assert!(revisions.list("m", 10).unwrap().is_empty());
    }
}
