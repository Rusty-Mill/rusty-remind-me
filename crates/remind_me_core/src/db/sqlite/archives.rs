//! Import archives and their spans on SQLite: the [`ArchiveStore`] adapter.

use super::{exec, SqliteTables};
use crate::db::archives::{ArchiveRow, ArchiveStore, SpanSource};
use crate::db::Result;
use rusqlite::params;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS import_archives (
    import_id TEXT PRIMARY KEY, hash TEXT NOT NULL, filename TEXT NOT NULL,
    archive_path TEXT NOT NULL, byte_len INTEGER NOT NULL, archived_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS import_archives_hash ON import_archives (hash);
CREATE TABLE IF NOT EXISTS import_archive_spans (
    memory_id TEXT PRIMARY KEY, import_id TEXT NOT NULL,
    byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS import_archive_spans_import ON import_archive_spans (import_id);
";

fn count(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

impl ArchiveStore for SqliteTables {
    fn record(&mut self, r: &ArchiveRow) -> Result<()> {
        self.exec(
            "INSERT OR REPLACE INTO import_archives VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                r.import_id,
                r.hash,
                r.filename,
                r.archive_path,
                r.byte_len,
                r.archived_at
            ],
        )
        .map(drop)
    }

    fn record_span(
        &mut self,
        memory_id: &str,
        import_id: &str,
        start: i64,
        end: i64,
    ) -> Result<()> {
        self.exec(
            "INSERT OR REPLACE INTO import_archive_spans VALUES (?1, ?2, ?3, ?4)",
            params![memory_id, import_id, start, end],
        )
        .map(drop)
    }

    fn span_source(&self, memory_id: &str) -> Result<Option<SpanSource>> {
        self.one(
            "SELECT s.import_id, s.byte_start, s.byte_end, a.archive_path, a.filename \
             FROM import_archive_spans s JOIN import_archives a ON a.import_id = s.import_id \
             WHERE s.memory_id = ?1",
            [memory_id],
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
    }

    fn blob_of(&self, import_id: &str) -> Result<Option<(String, String)>> {
        self.one(
            "SELECT archive_path, hash FROM import_archives WHERE import_id = ?1",
            [import_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    fn remove(&mut self, import_id: &str) -> Result<()> {
        self.atomic(|c| {
            exec(
                c,
                "DELETE FROM import_archive_spans WHERE import_id = ?1",
                [import_id],
            )?;
            exec(
                c,
                "DELETE FROM import_archives WHERE import_id = ?1",
                [import_id],
            )
            .map(drop)
        })
    }

    fn count_with_hash(&self, hash: &str) -> Result<usize> {
        let n = self.one(
            "SELECT count(*) FROM import_archives WHERE hash = ?1",
            [hash],
            |r| r.get(0),
        )?;
        Ok(count(n.unwrap_or(0)))
    }

    fn oldest_first(&self) -> Result<Vec<ArchiveRow>> {
        self.all(
            "SELECT import_id, hash, filename, archive_path, byte_len, archived_at \
             FROM import_archives ORDER BY archived_at, import_id",
            [],
            |r| {
                Ok(ArchiveRow {
                    import_id: r.get(0)?,
                    hash: r.get(1)?,
                    filename: r.get(2)?,
                    archive_path: r.get(3)?,
                    byte_len: r.get(4)?,
                    archived_at: r.get(5)?,
                })
            },
        )
    }

    fn span_count(&self, import_id: Option<&str>) -> Result<usize> {
        let n = self.one(
            "SELECT count(*) FROM import_archive_spans WHERE ?1 IS NULL OR import_id = ?1",
            [import_id],
            |r| r.get(0),
        )?;
        Ok(count(n.unwrap_or(0)))
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::archives::archive_conformance!(SqliteTables::open_in_memory().unwrap());
}
