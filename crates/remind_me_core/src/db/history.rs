//! Storage for per-memory edit history: `memory_revisions`, and the tracked
//! columns of `memories` a revision snapshots and a revert writes back.
//!
//! Every statement for history lives here (ADR-0022). The rules stay in
//! [`crate::history`]: which columns are tracked, what counts as a change,
//! and that a revert is itself a revisioned edit.

use crate::models::MemoryRevision;
use rusqlite::{params, Connection, OptionalExtension, Result};

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

/// The revision tables, over one connection.
pub struct Revisions<'c> {
    conn: &'c Connection,
}

impl<'c> Revisions<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Whether `memory_id` names a memory that is not deleted.
    pub fn is_live(&self, memory_id: &str) -> Result<bool> {
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
        self.conn
            .query_row(
                "SELECT content, category, tags, metadata, sensitive
                   FROM memories WHERE id = ?",
                params![memory_id],
                read_tracked,
            )
            .optional()
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
            .collect();
        rows
    }

    /// The tracked values revision `revision_id` holds, if it belongs to
    /// `memory_id`.
    pub fn revision(&self, memory_id: &str, revision_id: i64) -> Result<Option<Tracked>> {
        self.conn
            .query_row(
                "SELECT content, category, tags, metadata, sensitive
                   FROM memory_revisions WHERE id = ? AND memory_id = ?",
                params![revision_id, memory_id],
                read_tracked,
            )
            .optional()
    }

    /// Write `values` into `memory_id`'s tracked columns, stamping
    /// `updated_at`. A `None` `sensitive` is written as not sensitive.
    pub fn restore(&self, memory_id: &str, values: &Tracked, updated_at: &str) -> Result<()> {
        self.conn.execute(
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
        )?;
        Ok(())
    }
}

fn read_tracked(r: &rusqlite::Row<'_>) -> Result<Tracked> {
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
