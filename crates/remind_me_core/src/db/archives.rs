//! Storage for raw-transcript retention: `import_archives` (one row per
//! archived import) and `import_archive_spans` (which bytes of it produced
//! each memory).
//!
//! ADR-0023 phase 1, step 5b. Every statement [`crate::archive`] ran lives
//! here. The rules stay there: whether retention is on, where blobs go,
//! span clamping, the sensitive-memory gate, and pruning by age and size.

use rusqlite::{params, Connection, OptionalExtension, Result};

/// Create both tables and the span index, if absent.
///
/// No foreign key to `chat_imports`. The rows outlive an interrupted import
/// on purpose, and cleanup needs to read `archive_path` before the row goes:
/// a cascade would delete the row and orphan the file.
pub fn ensure_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS import_archives (
            import_id    TEXT PRIMARY KEY,
            hash         TEXT NOT NULL,
            filename     TEXT NOT NULL,
            archive_path TEXT NOT NULL,
            byte_len     INTEGER NOT NULL,
            archived_at  TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS import_archive_spans (
            memory_id  TEXT PRIMARY KEY,
            import_id  TEXT NOT NULL,
            byte_start INTEGER NOT NULL,
            byte_end   INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_archive_spans_import
            ON import_archive_spans(import_id);",
    )
}

/// One archived import.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveRow {
    pub import_id: String,
    pub hash: String,
    pub filename: String,
    pub archive_path: String,
    pub byte_len: i64,
    pub archived_at: String,
}

/// Where one memory's bytes are: its import, the span, and the archive.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanSource {
    pub import_id: String,
    pub byte_start: i64,
    pub byte_end: i64,
    pub archive_path: String,
    pub filename: String,
}

/// The retention tables, over one connection.
pub struct Archives<'c> {
    conn: &'c Connection,
}

impl<'c> Archives<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Record `row`, replacing any archive already recorded for its import.
    pub fn record(&self, row: &ArchiveRow) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO import_archives
                (import_id, hash, filename, archive_path, byte_len, archived_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                row.import_id,
                row.hash,
                row.filename,
                row.archive_path,
                row.byte_len,
                row.archived_at
            ],
        )?;
        Ok(())
    }

    /// Record that bytes `byte_start..byte_end` of `import_id` produced
    /// `memory_id`, replacing any span recorded for it.
    pub fn record_span(
        &self,
        memory_id: &str,
        import_id: &str,
        byte_start: i64,
        byte_end: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO import_archive_spans
                (memory_id, import_id, byte_start, byte_end)
             VALUES (?, ?, ?, ?)",
            params![memory_id, import_id, byte_start, byte_end],
        )?;
        Ok(())
    }

    /// Where `memory_id`'s bytes are, if it has a span in a recorded archive.
    pub fn span_source(&self, memory_id: &str) -> Result<Option<SpanSource>> {
        self.conn
            .query_row(
                "SELECT s.import_id, s.byte_start, s.byte_end, a.archive_path, a.filename
                   FROM import_archive_spans s
                   JOIN import_archives a ON a.import_id = s.import_id
                  WHERE s.memory_id = ?",
                params![memory_id],
                |r| {
                    Ok(SpanSource {
                        import_id: r.get(0)?,
                        byte_start: r.get(1)?,
                        byte_end: r.get(2)?,
                        archive_path: r.get(3)?,
                        filename: r.get(4)?,
                    })
                },
            )
            .optional()
    }

    /// The archive path and content hash recorded for `import_id`.
    pub fn blob_of(&self, import_id: &str) -> Result<Option<(String, String)>> {
        self.conn
            .query_row(
                "SELECT archive_path, hash FROM import_archives WHERE import_id = ?",
                params![import_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// Remove `import_id`'s archive row and spans. The blob is not touched.
    pub fn remove(&self, import_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM import_archive_spans WHERE import_id = ?",
            params![import_id],
        )?;
        self.conn.execute(
            "DELETE FROM import_archives WHERE import_id = ?",
            params![import_id],
        )?;
        Ok(())
    }

    /// How many recorded archives share the blob `hash`.
    pub fn count_with_hash(&self, hash: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM import_archives WHERE hash = ?",
            params![hash],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Every recorded archive, oldest `archived_at` first.
    pub fn oldest_first(&self) -> Result<Vec<ArchiveRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT import_id, hash, filename, archive_path, byte_len, archived_at
               FROM import_archives
              ORDER BY archived_at ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ArchiveRow {
                    import_id: r.get(0)?,
                    hash: r.get(1)?,
                    filename: r.get(2)?,
                    archive_path: r.get(3)?,
                    byte_len: r.get(4)?,
                    archived_at: r.get(5)?,
                })
            })?
            .collect();
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn row(import_id: &str, hash: &str, archived_at: &str) -> ArchiveRow {
        ArchiveRow {
            import_id: import_id.to_string(),
            hash: hash.to_string(),
            filename: "chat.json".to_string(),
            archive_path: format!("/archive/{hash}"),
            byte_len: 10,
            archived_at: archived_at.to_string(),
        }
    }

    #[test]
    fn remove_takes_the_spans_and_leaves_a_shared_blob_counted() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let archives = Archives::new(&conn);
        archives.record(&row("a", "h", "2026-09-02")).unwrap();
        archives.record(&row("b", "h", "2026-09-01")).unwrap();
        archives.record_span("m1", "a", 0, 5).unwrap();

        assert_eq!(archives.span_source("m1").unwrap().unwrap().import_id, "a");
        archives.remove("a").unwrap();
        assert!(archives.span_source("m1").unwrap().is_none());
        assert_eq!(archives.count_with_hash("h").unwrap(), 1);
        let ids: Vec<String> = archives
            .oldest_first()
            .unwrap()
            .into_iter()
            .map(|r| r.import_id)
            .collect();
        assert_eq!(ids, vec!["b"]);
    }
}
