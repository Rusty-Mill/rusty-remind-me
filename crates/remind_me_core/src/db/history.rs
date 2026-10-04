//! Storage for per-memory edit history: `memory_revisions`
//! (`db::engine::revisions`), and the tracked columns of `memories` a
//! revision snapshots and a revert writes back (`db::engine::memories`).
//!
//! Every read and write for history goes through here (ADR-0022). The rules
//! stay in [`crate::history`]: which columns are tracked, what counts as a
//! change, and that a revert is itself a revisioned edit.

use super::engine::{self, EngineLock};
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
}

/// The revision table and the tracked memory columns, on the engine.
pub struct Revisions<'c> {
    engine: &'c EngineLock,
}

impl<'c> Revisions<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            engine: store.engine(),
        }
    }

    /// Whether `memory_id` names a memory that is not deleted.
    pub fn is_live(&self, memory_id: &str) -> Result<bool> {
        Ok(engine::memories::get_live(&self.engine.lock(), memory_id)?.is_some())
    }

    /// `memory_id`'s tracked columns as stored now, deleted or not, or `None`
    /// if there is no such memory.
    pub fn current(&self, memory_id: &str) -> Result<Option<Tracked>> {
        engine::memories::tracked(&self.engine.lock(), memory_id)
    }

    /// Remove every revision of `memory_id`: a deleted memory's text must
    /// not stay readable in its history (ADR-0024).
    pub fn delete_for(&self, memory_id: &str) -> Result<()> {
        engine::revisions::delete_for(&mut self.engine.lock(), memory_id)
    }

    /// Remove the revisions of every tombstoned memory, as a delete now
    /// does, for the memories deleted before it did. How many went.
    pub fn delete_of_tombstones(&self) -> Result<usize> {
        engine::revisions::delete_of_tombstones(&mut self.engine.lock())
    }

    /// Append a revision of `memory_id` holding `values`, edited at
    /// `edited_at`.
    pub fn insert(
        &self,
        memory_id: &str,
        values: &Tracked,
        edited_at: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        engine::revisions::insert(
            &mut self.engine.lock(),
            memory_id,
            values,
            edited_at,
            reason,
        )
    }

    /// `memory_id`'s revisions, newest first, at most `limit`.
    ///
    /// Ordered by `edited_at` then `id`, so revisions captured within the
    /// same clock tick still list in the order they were written.
    pub fn list(&self, memory_id: &str, limit: usize) -> Result<Vec<MemoryRevision>> {
        Ok(engine::revisions::list(
            &self.engine.lock(),
            memory_id,
            limit,
        ))
    }

    /// The tracked values revision `revision_id` holds, if it belongs to
    /// `memory_id`.
    pub fn revision(&self, memory_id: &str, revision_id: i64) -> Result<Option<Tracked>> {
        Ok(engine::revisions::revision(
            &self.engine.lock(),
            memory_id,
            revision_id,
        ))
    }

    /// Write `values` into `memory_id`'s tracked columns, stamping
    /// `updated_at`. A `None` `sensitive` is written as not sensitive.
    pub fn restore(&self, memory_id: &str, values: &Tracked, updated_at: &str) -> Result<()> {
        engine::memories::restore_tracked(&mut self.engine.lock(), memory_id, values, updated_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn values(content: &str) -> Tracked {
        Tracked {
            content: content.to_string(),
            category: "fact".to_string(),
            tags: "[]".to_string(),
            metadata: "{}".to_string(),
            sensitive: Some(true),
        }
    }

    #[test]
    fn revisions_list_newest_first_and_belong_to_their_memory() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let revisions = Revisions::new(&store);
        let same_tick = "2026-09-27T00:00:01+00:00";
        revisions
            .insert("m1", &values("a"), "2026-09-27T00:00:00+00:00", None)
            .unwrap();
        revisions
            .insert("m1", &values("b"), same_tick, None)
            .unwrap();
        revisions
            .insert("m1", &values("c"), same_tick, Some("revert"))
            .unwrap();
        revisions
            .insert("m2", &values("other"), same_tick, None)
            .unwrap();

        let listed = revisions.list("m1", 10).unwrap();
        let contents: Vec<&str> = listed.iter().map(|r| r.content.as_str()).collect();
        assert_eq!(contents, ["c", "b", "a"], "same tick: last written first");
        assert_eq!(listed[0].revision_reason.as_deref(), Some("revert"));
        assert_eq!(listed[0].sensitive, Some(true));
        assert_eq!(revisions.list("m1", 2).unwrap().len(), 2);

        let oldest = listed[2].id;
        assert_eq!(revisions.revision("m1", oldest).unwrap(), Some(values("a")));
        assert_eq!(
            revisions.revision("m2", oldest).unwrap(),
            None,
            "a revision id from another memory is not found"
        );
    }
}
