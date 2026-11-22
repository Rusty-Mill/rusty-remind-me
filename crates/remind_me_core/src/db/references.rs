//! Storage for `memory_references` (schema v32): the URLs, issues, pull
//! requests, commits, paths, handles and attachments a memory names, each
//! as a normalised `(kind, value)` so two memories naming the same thing
//! meet on it.
//!
//! Storage only. What counts as a reference and how a value is normalised
//! (`owner/repo#123`, a full sha, `sha256:<hex>` for an attachment) is the
//! detector's business, and nothing here is queued for sync.
//!
//! Added at schema v32, once the engine was the node's store (ADR-0023),
//! so the table never lived in SQLite and the copy of an old `memory.db`
//! has nothing to read for it.
//!
//! [`ReferenceStore`] is the port (ADR-0035); the engine's adapter is in
//! `db::engine::references`. [`References`] is what callers take.

use super::backend::BackendLock;
use super::{Result, Store};
use serde::{Deserialize, Serialize};

/// A reference to store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewReference {
    /// `ref_<uuid simple>`; see [`NewReference::new`].
    pub id: String,
    pub memory_id: String,
    /// `url`, `issue`, `pull`, `commit`, `path`, `handle` or `attachment`.
    pub kind: String,
    /// The normalised value.
    pub value: String,
    /// Display text, or an attachment's original filename.
    pub label: Option<String>,
    pub created_at: String,
}

impl NewReference {
    /// A reference with a fresh id and no label.
    pub fn new(
        memory_id: impl Into<String>,
        kind: impl Into<String>,
        value: impl Into<String>,
        created_at: &str,
    ) -> Self {
        Self {
            id: format!("ref_{}", uuid::Uuid::new_v4().simple()),
            memory_id: memory_id.into(),
            kind: kind.into(),
            value: value.into(),
            label: None,
            created_at: created_at.to_string(),
        }
    }
}

/// A stored reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryReference {
    pub id: String,
    pub memory_id: String,
    pub kind: String,
    pub value: String,
    pub label: Option<String>,
    pub created_at: String,
}

/// The `memory_references` table, as a backend stores it (ADR-0035).
/// [`conformance`] holds the behaviour every implementation must show.
pub trait ReferenceStore {
    /// Insert `row`. An existing id is an error.
    fn insert(&mut self, row: &NewReference) -> Result<()>;

    /// Every reference of memory `memory_id`, oldest first (ties by kind,
    /// value, then id).
    fn for_memory(&self, memory_id: &str) -> Result<Vec<MemoryReference>>;

    /// The references of all of `memory_ids` in one pass (no N+1), in the
    /// same order; regroup by `memory_id`.
    fn for_memories(&self, memory_ids: &[String]) -> Result<Vec<MemoryReference>>;

    /// Every reference of this `kind`, in the same order.
    fn of_kind(&self, kind: &str) -> Result<Vec<MemoryReference>>;

    /// Every reference with exactly this `kind` and `value`, in the same
    /// order: the memories that name one thing.
    fn find(&self, kind: &str, value: &str) -> Result<Vec<MemoryReference>>;

    /// Remove every reference of memory `memory_id`. How many went.
    fn delete_for_memory(&mut self, memory_id: &str) -> Result<usize>;
}

/// The `memory_references` table of a [`Store`], one locked call at a time.
pub struct References<'c> {
    backend: &'c BackendLock,
}

impl<'c> References<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            backend: store.backend(),
        }
    }

    /// See [`ReferenceStore::insert`].
    pub fn insert(&self, row: &NewReference) -> Result<()> {
        ReferenceStore::insert(&mut *self.backend.lock()?, row)
    }

    /// See [`ReferenceStore::for_memory`].
    pub fn for_memory(&self, memory_id: &str) -> Result<Vec<MemoryReference>> {
        ReferenceStore::for_memory(&*self.backend.lock()?, memory_id)
    }

    /// See [`ReferenceStore::for_memories`].
    pub fn for_memories(&self, memory_ids: &[String]) -> Result<Vec<MemoryReference>> {
        ReferenceStore::for_memories(&*self.backend.lock()?, memory_ids)
    }

    /// See [`ReferenceStore::of_kind`].
    pub fn of_kind(&self, kind: &str) -> Result<Vec<MemoryReference>> {
        ReferenceStore::of_kind(&*self.backend.lock()?, kind)
    }

    /// See [`ReferenceStore::find`].
    pub fn find(&self, kind: &str, value: &str) -> Result<Vec<MemoryReference>> {
        ReferenceStore::find(&*self.backend.lock()?, kind, value)
    }

    /// See [`ReferenceStore::delete_for_memory`].
    pub fn delete_for_memory(&self, memory_id: &str) -> Result<usize> {
        ReferenceStore::delete_for_memory(&mut *self.backend.lock()?, memory_id)
    }
}

/// The behaviour every [`ReferenceStore`] must show, run against each
/// backend by [`reference_conformance`] (ADR-0035).
#[doc(hidden)]
pub mod conformance {
    use super::{MemoryReference, NewReference, ReferenceStore};

    const T1: &str = "2026-09-26T00:00:00+00:00";
    const T2: &str = "2026-09-27T00:00:00+00:00";

    fn reference(id: &str, memory_id: &str, kind: &str, value: &str, at: &str) -> NewReference {
        NewReference {
            id: id.to_string(),
            label: Some(format!("label {id}")),
            ..NewReference::new(memory_id, kind, value, at)
        }
    }

    fn ids(found: Vec<MemoryReference>) -> Vec<String> {
        found.into_iter().map(|r| r.id).collect()
    }

    /// Three references, inserted out of time order to check reads order
    /// them.
    fn seed(store: &mut dyn ReferenceStore) {
        store
            .insert(&reference("r2", "m1", "issue", "o/r#1", T2))
            .unwrap();
        store
            .insert(&reference("r1", "m1", "url", "https://x", T1))
            .unwrap();
        store
            .insert(&reference("r3", "m2", "issue", "o/r#1", T1))
            .unwrap();
    }

    pub fn round_trips_every_field(store: &mut dyn ReferenceStore) {
        seed(store);
        assert_eq!(
            store.for_memory("m1").unwrap()[0],
            MemoryReference {
                id: "r1".into(),
                memory_id: "m1".into(),
                kind: "url".into(),
                value: "https://x".into(),
                label: Some("label r1".into()),
                created_at: T1.into(),
            }
        );
    }

    pub fn a_taken_id_is_refused(store: &mut dyn ReferenceStore) {
        seed(store);
        assert!(store
            .insert(&reference("r1", "m9", "url", "dup", T1))
            .is_err());
        assert!(store.for_memory("m9").unwrap().is_empty());
    }

    pub fn reads_are_oldest_first(store: &mut dyn ReferenceStore) {
        seed(store);
        assert_eq!(ids(store.for_memory("m1").unwrap()), ["r1", "r2"]);
        assert_eq!(
            ids(store.for_memories(&["m2".into(), "m1".into()]).unwrap()),
            ["r3", "r1", "r2"],
            "ties on time break by kind, then value"
        );
        assert_eq!(ids(store.of_kind("issue").unwrap()), ["r3", "r2"]);
        assert!(store.for_memory("missing").unwrap().is_empty());
        assert!(store.for_memories(&[]).unwrap().is_empty());
    }

    pub fn find_matches_kind_and_value_exactly(store: &mut dyn ReferenceStore) {
        seed(store);
        assert_eq!(ids(store.find("issue", "o/r#1").unwrap()), ["r3", "r2"]);
        assert!(store.find("issue", "o/r#2").unwrap().is_empty());
        assert!(store.find("url", "o/r#1").unwrap().is_empty());
    }

    pub fn delete_for_memory_counts_what_went(store: &mut dyn ReferenceStore) {
        seed(store);
        assert_eq!(store.delete_for_memory("m1").unwrap(), 2);
        assert_eq!(store.delete_for_memory("m1").unwrap(), 0);
        assert!(store.for_memory("m1").unwrap().is_empty());
        assert_eq!(ids(store.find("issue", "o/r#1").unwrap()), ["r3"]);
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`. A backend's adapter invokes it in its own test module.
#[doc(hidden)]
#[macro_export]
macro_rules! reference_conformance {
    ($make:expr) => {
        use $crate::db::references::conformance;

        #[test]
        fn round_trips_every_field() {
            conformance::round_trips_every_field(&mut $make);
        }
        #[test]
        fn a_taken_id_is_refused() {
            conformance::a_taken_id_is_refused(&mut $make);
        }
        #[test]
        fn reads_are_oldest_first() {
            conformance::reads_are_oldest_first(&mut $make);
        }
        #[test]
        fn find_matches_kind_and_value_exactly() {
            conformance::find_matches_kind_and_value_exactly(&mut $make);
        }
        #[test]
        fn delete_for_memory_counts_what_went() {
            conformance::delete_for_memory_counts_what_went(&mut $make);
        }
    };
}
#[doc(hidden)]
pub use crate::reference_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    const T1: &str = "2026-09-26T00:00:00+00:00";

    #[test]
    fn new_mints_a_ref_id_with_no_label() {
        let r = NewReference::new("m1", "url", "https://example.org", T1);
        assert!(r.id.starts_with("ref_"), "{}", r.id);
        assert_eq!(r.id.len(), "ref_".len() + 32);
        assert_eq!(r.label, None);
        assert_ne!(r.id, NewReference::new("m1", "url", "x", T1).id);
    }

    #[test]
    fn the_repository_reaches_the_store_through_the_port() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let refs = References::new(&store);
        refs.insert(&NewReference::new("m1", "url", "https://x", T1))
            .unwrap();
        assert_eq!(refs.for_memory("m1").unwrap().len(), 1);
        assert_eq!(refs.for_memories(&["m1".into()]).unwrap().len(), 1);
        assert_eq!(refs.of_kind("url").unwrap().len(), 1);
        assert_eq!(refs.find("url", "https://x").unwrap().len(), 1);
        assert_eq!(refs.delete_for_memory("m1").unwrap(), 1);
    }
}
