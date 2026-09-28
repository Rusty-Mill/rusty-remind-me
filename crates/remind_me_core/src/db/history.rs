//! Storage for per-memory edit history: `memory_revisions`, and the tracked
//! columns of `memories` a revision snapshots and a revert writes back.
//!
//! Every statement for history lives here (ADR-0022). The rules stay in
//! [`crate::history`]: which columns are tracked, what counts as a change,
//! and that a revert is itself a revisioned edit.
//!
//! With the `engine-store` feature, a store that carries engine tables keeps
//! `memory_revisions` there (`db::engine::revisions`, ADR-0023 phase 4g).
//! The reads and writes of `memories` itself stay on SQLite until memories
//! move.

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineLock};
use super::{Result, Store};
use crate::db::derived::{write_memory, Origin};
use crate::models::MemoryRevision;
use rusqlite::{params, Connection, OptionalExtension};

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

/// The revision tables, over one store.
pub struct Revisions<'c> {
    conn: &'c Connection,
    #[cfg(feature = "engine-store")]
    engine: Option<&'c EngineLock>,
    /// The tables again when they hold the memories core.
    #[cfg(feature = "engine-store")]
    core: Option<&'c EngineLock>,
}

impl<'c> Revisions<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            #[cfg(feature = "engine-store")]
            engine: store.engine(),
            #[cfg(feature = "engine-store")]
            core: store.core(),
        }
    }

    /// Whether `memory_id` names a memory that is not deleted.
    pub fn is_live(&self, memory_id: &str) -> Result<bool> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return Ok(engine::memories::get_live(&core.lock(), memory_id)?.is_some());
        }
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM memories WHERE id = ? AND deleted_at IS NULL",
                params![memory_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// `memory_id`'s tracked columns as stored now, deleted or not, or `None`
    /// if there is no such memory.
    pub fn current(&self, memory_id: &str) -> Result<Option<Tracked>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::memories::tracked(&core.lock(), memory_id);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT content, category, tags, metadata, sensitive
                   FROM memories WHERE id = ?",
                params![memory_id],
                read_tracked,
            )
            .optional()?)
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
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::revisions::insert(
                &mut engine.lock(),
                memory_id,
                values,
                edited_at,
                reason,
            );
        }
        self.conn.execute(
            "INSERT INTO memory_revisions
                 (memory_id, content, category, tags, metadata, sensitive,
                  edited_at, revision_reason)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                memory_id,
                values.content,
                values.category,
                values.tags,
                values.metadata,
                values.sensitive.map(i64::from),
                edited_at,
                reason,
            ],
        )?;
        Ok(())
    }

    /// `memory_id`'s revisions, newest first, at most `limit`.
    ///
    /// Ordered by `edited_at` then `id`, so revisions captured within the
    /// same clock tick still list in the order they were written.
    pub fn list(&self, memory_id: &str, limit: usize) -> Result<Vec<MemoryRevision>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::revisions::list(&engine.lock(), memory_id, limit));
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, memory_id, content, category, tags, metadata, sensitive,
                    edited_at, revision_reason
               FROM memory_revisions
              WHERE memory_id = ?
              ORDER BY edited_at DESC, id DESC
              LIMIT ?",
        )?;
        // A limit past `i64::MAX` is unbounded either way.
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(params![memory_id, limit], |r| {
                Ok(MemoryRevision {
                    id: r.get(0)?,
                    memory_id: r.get(1)?,
                    content: r.get(2)?,
                    category: r.get(3)?,
                    tags: r.get(4)?,
                    metadata: r.get(5)?,
                    sensitive: r.get::<_, Option<i64>>(6)?.map(|v| v != 0),
                    edited_at: r.get(7)?,
                    revision_reason: r.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// The tracked values revision `revision_id` holds, if it belongs to
    /// `memory_id`.
    pub fn revision(&self, memory_id: &str, revision_id: i64) -> Result<Option<Tracked>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::revisions::revision(
                &engine.lock(),
                memory_id,
                revision_id,
            ));
        }
        Ok(self
            .conn
            .query_row(
                "SELECT content, category, tags, metadata, sensitive
                   FROM memory_revisions WHERE id = ? AND memory_id = ?",
                params![revision_id, memory_id],
                read_tracked,
            )
            .optional()?)
    }

    /// Write `values` into `memory_id`'s tracked columns, stamping
    /// `updated_at`. A `None` `sensitive` is written as not sensitive.
    pub fn restore(&self, memory_id: &str, values: &Tracked, updated_at: &str) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::memories::restore_tracked(
                &mut core.lock(),
                memory_id,
                values,
                updated_at,
            );
        }
        write_memory(self.conn, memory_id, Origin::Local, || {
            self.write_tracked(memory_id, values, updated_at)
        })?;
        Ok(())
    }

    fn write_tracked(&self, memory_id: &str, values: &Tracked, updated_at: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE memories
                SET content = ?, category = ?, tags = ?, metadata = ?,
                    sensitive = ?, updated_at = ?
              WHERE id = ?",
            params![
                values.content,
                values.category,
                values.tags,
                values.metadata,
                i64::from(values.sensitive.unwrap_or(false)),
                updated_at,
                memory_id
            ],
        )?)
    }
}

fn read_tracked(r: &rusqlite::Row<'_>) -> rusqlite::Result<Tracked> {
    Ok(Tracked {
        content: r.get(0)?,
        category: r.get(1)?,
        tags: r.get(2)?,
        metadata: r.get(3)?,
        // Tolerant: an unreadable value is "unknown", not an error, so an old
        // revision stays revertable.
        sensitive: r.get::<_, i64>(4).ok().map(|v| v != 0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::on_each_backend;

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
        on_each_backend(|db| {
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
        });
    }
}
