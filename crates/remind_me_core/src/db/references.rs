//! Storage for `memory_references` (schema v32): the URLs, issues, pull
//! requests, commits, paths, handles and attachments a memory names, each
//! as a normalised `(kind, value)` so two memories naming the same thing
//! meet on it.
//!
//! Storage only. What counts as a reference and how a value is normalised
//! (`owner/repo#123`, a full sha, `sha256:<hex>` for an attachment) is the
//! detector's business, and nothing here is queued for sync.
//!
//! Engine only: the table was added once the engine was the node's store,
//! and SQLite stays only to open and copy an older database onto it
//! (ADR-0023). A store on SQLite answers every call here with
//! [`StoreError::Invalid`].

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineLock};
use super::{Result, Store, StoreError};
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

/// The error every call answers with on a store without the engine tables.
pub(crate) fn not_on_sqlite(table: &str) -> StoreError {
    StoreError::Invalid(format!(
        "{table} is not supported on the legacy SQLite store; open the node on the engine"
    ))
}

/// The `memory_references` table on the engine's memories core
/// (`db::engine::references`).
pub struct References<'c> {
    #[cfg(feature = "engine-store")]
    core: Option<&'c EngineLock>,
    #[cfg(not(feature = "engine-store"))]
    _store: std::marker::PhantomData<&'c ()>,
}

impl<'c> References<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        #[cfg(not(feature = "engine-store"))]
        let _ = store;
        Self {
            #[cfg(feature = "engine-store")]
            core: store.core(),
            #[cfg(not(feature = "engine-store"))]
            _store: std::marker::PhantomData,
        }
    }

    #[cfg(feature = "engine-store")]
    fn core(&self) -> Result<&'c EngineLock> {
        self.core.ok_or_else(|| not_on_sqlite("memory_references"))
    }

    #[cfg(not(feature = "engine-store"))]
    fn core(&self) -> Result<std::convert::Infallible> {
        Err(not_on_sqlite("memory_references"))
    }

    /// Insert `row`. An existing id is an error.
    pub fn insert(&self, row: &NewReference) -> Result<()> {
        let core = self.core()?;
        #[cfg(feature = "engine-store")]
        return engine::references::insert(&mut core.lock(), row);
        #[cfg(not(feature = "engine-store"))]
        match core {}
    }

    /// Every reference of memory `memory_id`, oldest first (ties by id).
    pub fn for_memory(&self, memory_id: &str) -> Result<Vec<MemoryReference>> {
        let core = self.core()?;
        #[cfg(feature = "engine-store")]
        return engine::references::for_memory(&core.lock(), memory_id);
        #[cfg(not(feature = "engine-store"))]
        match core {}
    }

    /// Every reference with exactly this `kind` and `value`, oldest first
    /// (ties by id): the memories that name one thing.
    pub fn find(&self, kind: &str, value: &str) -> Result<Vec<MemoryReference>> {
        let core = self.core()?;
        #[cfg(feature = "engine-store")]
        return engine::references::find(&core.lock(), kind, value);
        #[cfg(not(feature = "engine-store"))]
        match core {}
    }

    /// Remove every reference of memory `memory_id`. How many went.
    pub fn delete_for_memory(&self, memory_id: &str) -> Result<usize> {
        let core = self.core()?;
        #[cfg(feature = "engine-store")]
        return engine::references::delete_for_memory(&mut core.lock(), memory_id);
        #[cfg(not(feature = "engine-store"))]
        match core {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    const T1: &str = "2026-09-26T00:00:00+00:00";
    const T2: &str = "2026-09-27T00:00:00+00:00";

    fn reference(id: &str, memory_id: &str, kind: &str, value: &str, at: &str) -> NewReference {
        NewReference {
            id: id.to_string(),
            label: Some(format!("label {id}")),
            ..NewReference::new(memory_id, kind, value, at)
        }
    }

    #[test]
    fn new_mints_a_ref_id_with_no_label() {
        let r = NewReference::new("m1", "url", "https://example.org", T1);
        assert!(r.id.starts_with("ref_"), "{}", r.id);
        assert_eq!(r.id.len(), "ref_".len() + 32);
        assert_eq!(r.label, None);
        assert_ne!(r.id, NewReference::new("m1", "url", "x", T1).id);
    }

    #[test]
    fn the_sqlite_store_refuses_references() {
        let db = Database::open_sqlite_in_memory().unwrap();
        let store = db.store();
        let refused = References::new(&store).for_memory("m1");
        assert!(
            matches!(&refused, Err(StoreError::Invalid(why)) if why.contains("legacy SQLite")),
            "{refused:?}"
        );
    }

    #[cfg(feature = "engine-store")]
    #[test]
    fn references_are_stored_found_and_deleted() {
        {
            let db = Database::open_in_memory_on_engine().unwrap();
            let store = db.store();
            let refs = References::new(&store);
            // Inserted out of time order, to check the read orders them.
            refs.insert(&reference("r2", "m1", "issue", "o/r#1", T2))
                .unwrap();
            refs.insert(&reference("r1", "m1", "url", "https://x", T1))
                .unwrap();
            refs.insert(&reference("r3", "m2", "issue", "o/r#1", T1))
                .unwrap();
            assert!(
                refs.insert(&reference("r1", "m9", "url", "dup", T1))
                    .is_err(),
                "a taken id is refused"
            );

            let of_m1: Vec<(String, String)> = refs
                .for_memory("m1")
                .unwrap()
                .into_iter()
                .map(|r| (r.id, r.kind))
                .collect();
            assert_eq!(
                of_m1,
                [
                    ("r1".to_string(), "url".to_string()),
                    ("r2".to_string(), "issue".to_string())
                ]
            );
            assert_eq!(
                refs.for_memory("m1").unwrap()[0],
                MemoryReference {
                    id: "r1".into(),
                    memory_id: "m1".into(),
                    kind: "url".into(),
                    value: "https://x".into(),
                    label: Some("label r1".into()),
                    created_at: T1.into(),
                }
            );

            let naming_issue: Vec<String> = refs
                .find("issue", "o/r#1")
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect();
            assert_eq!(naming_issue, ["r3", "r2"], "oldest first");
            assert!(refs.find("issue", "o/r#2").unwrap().is_empty());
            assert!(refs.find("url", "o/r#1").unwrap().is_empty());
            assert!(refs.for_memory("missing").unwrap().is_empty());

            assert_eq!(refs.delete_for_memory("m1").unwrap(), 2);
            assert_eq!(refs.delete_for_memory("m1").unwrap(), 0);
            assert!(refs.for_memory("m1").unwrap().is_empty());
            assert_eq!(refs.find("issue", "o/r#1").unwrap().len(), 1);
        }
    }
}
