//! Raw transcript retention: keeping the bytes an import was derived from.
//!
//! # What this exists to recover
//!
//! [`crate::importer::extract_messages`] pulls `{role, content}` out of a chat
//! export and [`crate::importer`]'s `text_of` then drops `tool_use`,
//! `tool_result`, `thinking` and image blocks. That flattening is correct —
//! storing tool chatter as memories would bury recallable facts under
//! transcript noise — but it is also *terminal*: the discarded material is
//! gone, and the envelope's own fields (`uuid`, `parentUuid`, `sessionId`,
//! per-message timestamps, token usage) were never read at all.
//!
//! This module makes the flattening lossless at the file level. The import
//! still produces exactly the same memories; the source bytes are additionally
//! set aside, and each memory records the byte span it came from. Nothing here
//! changes what a search returns.
//!
//! # Off unless configured
//!
//! [`archive_root`] returns `None` unless [`ARCHIVE_DIR_ENV`] is set, matching
//! the folder watcher (#55), webhook (#56) and embedder convention: the
//! disk-consuming thing stays off until asked for. Every function below is a
//! no-op in that state, so an import with archiving off is byte-identical to
//! one from before this module existed.
//!
//! # Why its tables are not in the generated schema
//!
//! `db/schema_tables.sql` is generated verbatim from a `remind_me` database
//! and is not this crate's file to extend — see [`crate::db`]'s migrations
//! module. Adding an `archive_path` column to `chat_imports` would be silently
//! reverted by the next `scripts/regenerate_schema.py` run.
//!
//! So the two tables below are **target-only**, created by [`ensure_schema`]
//! at open time in the same way [`crate::vectors::ensure_schema`] creates
//! `vec_embeddings`. `migration_pending` only iterates tables present in the
//! pristine reference schema, so a table the reference has never heard of is
//! invisible to reconciliation rather than repeatedly rebuilt. A `remind_me`
//! sharing the database ignores them for the same reason.
//!
//! # Content-addressed, so a re-import costs nothing
//!
//! Blobs are stored at `<root>/<hash[0..2]>/<hash>` keyed on the same content
//! hash `chat_imports.hash` already uses. Importing the same file twice writes
//! one blob. That also makes the write idempotent, which matters because the
//! folder watcher re-scans the same directory forever.
//!
//! # Not synced
//!
//! Archives are node-local by construction: they are large, and they describe
//! files that exist on one machine. Nothing here is reachable from the sync
//! outbox — these tables carry no triggers, and `sync/` enumerates the
//! reference's tables, not this one's.

use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};
use std::path::{Path, PathBuf};

/// Directory raw transcripts are retained under. Unset disables retention.
pub const ARCHIVE_DIR_ENV: &str = "REMIND_ME_ARCHIVE_DIR";

/// Most bytes [`source_for`] will return in one call.
///
/// A session transcript can run to tens of megabytes, and the caller is
/// usually assembling an LLM context. Truncation is reported rather than
/// silent — see [`ArchiveSource::truncated`].
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;

/// The configured archive directory, or `None` when retention is off.
///
/// Read at call time rather than cached, matching [`crate::wiki_fs::Wiki`]'s
/// `from_env`, so a caller can relocate or disable it without restarting.
///
/// Deliberately **not** run through [`crate::import_paths::is_contained`].
/// That check guards paths an LLM can supply through a tool argument; this is
/// operator configuration at the same trust level as `REMIND_ME_DB_PATH` and
/// `REMIND_ME_WIKI_DIR`, neither of which is contained either.
pub fn archive_root() -> Option<PathBuf> {
    std::env::var(ARCHIVE_DIR_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| PathBuf::from(crate::import_paths::expand_home(&v)))
}

/// Whether raw transcripts are being retained.
pub fn is_enabled() -> bool {
    archive_root().is_some()
}

/// Create this crate's own archive tables, if they do not already exist.
///
/// Called from [`crate::db::schema::initialize_schema`] after the generated
/// schema is applied — the same arrangement as [`crate::vectors::ensure_schema`].
///
/// Created unconditionally, even with retention off. An empty table costs
/// nothing, and creating it lazily on first write would mean the read path had
/// to tolerate a missing table forever.
///
/// No foreign key to `chat_imports`. The rows outlive an interrupted import on
/// purpose, and cleanup needs to read `archive_path` *before* the row goes —
/// a cascade would delete the row and orphan the file, which is the failure
/// this table exists to make impossible. See [`forget_import`].
pub fn ensure_schema(conn: &Connection) -> SqlResult<()> {
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

/// Why a retention operation could not complete.
///
/// Every variant is something a caller degrades on rather than fails for: an
/// import whose archive could not be written is still a successful import.
#[derive(Debug)]
pub enum ArchiveError {
    Io(std::io::Error),
    Db(rusqlite::Error),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{}", e),
            Self::Db(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<std::io::Error> for ArchiveError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rusqlite::Error> for ArchiveError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

/// Where a blob for `hash` lives under `root`.
///
/// Two-character shard prefix, so a directory listing stays usable after a few
/// thousand imports.
fn blob_path(root: &Path, hash: &str) -> PathBuf {
    let shard = hash.get(..2).unwrap_or("__");
    root.join(shard).join(hash)
}

/// Retain `raw` as the source of `import_id`, if retention is on.
///
/// Returns the blob path when something was retained, `None` when retention is
/// off. Writing the blob is idempotent: an existing blob with this hash is
/// left alone rather than rewritten, since the hash is over these exact bytes.
pub fn store(
    conn: &Connection,
    import_id: &str,
    filename: &str,
    hash: &str,
    raw: &[u8],
) -> Result<Option<PathBuf>, ArchiveError> {
    let Some(root) = archive_root() else {
        return Ok(None);
    };

    let path = blob_path(&root, hash);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, raw)?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO import_archives
            (import_id, hash, filename, archive_path, byte_len, archived_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![
            import_id,
            hash,
            filename,
            path.to_string_lossy(),
            raw.len() as i64,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;

    // Enforce retention at the point of growth, so an archive cannot outrun
    // its ceiling between manual passes. A no-op unless a limit is set, and a
    // failure is swallowed for the same reason the write above is: retention
    // housekeeping must never fail the import it is decorating.
    let _ = prune(conn, false);

    Ok(Some(path))
}

/// Record which bytes of an archived import produced `memory_id`.
///
/// A no-op when retention is off, so callers do not have to check first.
pub fn record_span(
    conn: &Connection,
    memory_id: &str,
    import_id: &str,
    byte_start: usize,
    byte_end: usize,
) -> SqlResult<()> {
    if !is_enabled() {
        return Ok(());
    }
    conn.execute(
        "INSERT OR REPLACE INTO import_archive_spans
            (memory_id, import_id, byte_start, byte_end)
         VALUES (?, ?, ?, ?)",
        params![memory_id, import_id, byte_start as i64, byte_end as i64],
    )?;
    Ok(())
}

/// The raw source behind one memory.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArchiveSource {
    pub memory_id: String,
    pub import_id: String,
    /// The originating file's name, as imported.
    pub filename: String,
    /// Byte offsets of this memory's span within the archived file.
    pub byte_start: usize,
    pub byte_end: usize,
    /// The span's bytes, lossily decoded as UTF-8.
    pub content: String,
    /// Whether `content` was cut at [`MAX_SOURCE_BYTES`].
    pub truncated: bool,
}

/// The raw envelope(s) behind a memory, or `None` when there is no archived
/// source for it — retention was off at import time, this memory did not come
/// from a file import, or the blob has since been pruned.
///
/// A missing blob reads as `None` rather than an error: an archive is a cache
/// of something already distilled into memories, and losing it must never turn
/// a working read path into a failing one.
///
/// # Sensitive memories
///
/// A memory marked `sensitive` yields `None` unless `include_sensitive`.
/// The raw source is strictly *more* than the memory distilled from it — it
/// carries the tool calls, file paths and reasoning that the flattening
/// removed — so a flag meaning "do not surface this by default" has to cover
/// the larger disclosure at least as well as the smaller one.
///
/// This mirrors [`crate::models::MemorySearchInput::include_sensitive`] rather
/// than [`crate::digest`]'s no-override exclusion. Digest is ambient and
/// scheduled, with no per-call intent to opt back in against; this is a
/// by-id read, which is a deliberate act by a caller who already has the id.
pub fn source_for(
    conn: &Connection,
    memory_id: &str,
    include_sensitive: bool,
) -> SqlResult<Option<ArchiveSource>> {
    if !include_sensitive {
        let sensitive: Option<bool> = conn
            .query_row(
                "SELECT sensitive FROM memories WHERE id = ?",
                params![memory_id],
                |r| r.get(0),
            )
            .optional()?;
        if sensitive.unwrap_or(false) {
            return Ok(None);
        }
    }

    let row: Option<(String, i64, i64, String, String)> = conn
        .query_row(
            "SELECT s.import_id, s.byte_start, s.byte_end, a.archive_path, a.filename
               FROM import_archive_spans s
               JOIN import_archives a ON a.import_id = s.import_id
              WHERE s.memory_id = ?",
            params![memory_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;

    let Some((import_id, start, end, archive_path, filename)) = row else {
        return Ok(None);
    };

    let Ok(bytes) = std::fs::read(&archive_path) else {
        return Ok(None);
    };

    let start = (start.max(0) as usize).min(bytes.len());
    let end = (end.max(0) as usize).min(bytes.len());
    if end <= start {
        return Ok(None);
    }

    let span = &bytes[start..end];
    let truncated = span.len() > MAX_SOURCE_BYTES;
    let span = if truncated {
        &span[..MAX_SOURCE_BYTES]
    } else {
        span
    };

    Ok(Some(ArchiveSource {
        memory_id: memory_id.to_string(),
        import_id,
        filename,
        byte_start: start,
        byte_end: end,
        content: String::from_utf8_lossy(span).into_owned(),
        truncated,
    }))
}

/// Drop an import's archive: its spans, its row, and its blob.
///
/// Called from [`crate::undo_import`]. Undoing an import deletes the
/// `chat_imports` tracking row so the content becomes re-importable; leaving
/// the archive behind would strand a blob nothing references.
///
/// The blob is only unlinked when no *other* import still points at it —
/// content-addressed storage means two imports of the same file share one.
///
/// Returns the number of blobs actually removed.
pub fn forget_import(conn: &Connection, import_id: &str) -> SqlResult<usize> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT archive_path, hash FROM import_archives WHERE import_id = ?",
            params![import_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    conn.execute(
        "DELETE FROM import_archive_spans WHERE import_id = ?",
        params![import_id],
    )?;
    conn.execute(
        "DELETE FROM import_archives WHERE import_id = ?",
        params![import_id],
    )?;

    let Some((archive_path, hash)) = row else {
        return Ok(0);
    };

    let still_referenced: i64 = conn.query_row(
        "SELECT count(*) FROM import_archives WHERE hash = ?",
        params![hash],
        |r| r.get(0),
    )?;
    if still_referenced > 0 {
        return Ok(0);
    }

    Ok(usize::from(std::fs::remove_file(&archive_path).is_ok()))
}

/// Retain nothing older than this many days. Unset means no age limit.
pub const ARCHIVE_MAX_AGE_DAYS_ENV: &str = "REMIND_ME_ARCHIVE_MAX_AGE_DAYS";
/// Keep total archive bytes at or under this. Unset means no size limit.
pub const ARCHIVE_MAX_BYTES_ENV: &str = "REMIND_ME_ARCHIVE_MAX_BYTES";

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// What a prune pass did, or would do.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PruneReport {
    /// Archived imports considered.
    pub examined: usize,
    pub removed_for_age: usize,
    pub removed_for_size: usize,
    /// Distinct blob bytes freed. Two imports of one file share a blob, so
    /// this is disk reclaimed, not the sum of the rows' `byte_len`.
    pub bytes_reclaimed: u64,
    /// Distinct blob bytes still retained afterwards.
    pub bytes_remaining: u64,
    /// Whether either limit is configured at all. False means this pass could
    /// never remove anything, which is worth saying out loud — a report of
    /// zero removals otherwise reads as "nothing was old enough".
    pub limits_configured: bool,
    pub dry_run: bool,
}

/// One archived import, as retention sees it.
struct Retained {
    import_id: String,
    hash: String,
    byte_len: u64,
    archived_at: chrono::DateTime<chrono::Utc>,
}

/// Enforce the configured age and size ceilings.
///
/// # What "size" means here
///
/// Blobs are content-addressed, so two imports of the same file are two rows
/// pointing at one file. Totals are therefore summed over **distinct hashes**,
/// not over rows — summing rows would over-count a duplicated import and
/// evict history to reclaim bytes that were never on disk.
///
/// # Eviction order
///
/// Oldest first, by `archived_at`. That keeps the newest transcript — the one
/// most likely to be drilled into — and means a just-written archive survives
/// unless it alone exceeds the ceiling.
///
/// # Unset means unlimited
///
/// Neither limit has a default. Someone already running with
/// `REMIND_ME_ARCHIVE_DIR` set has an archive they chose to keep, and turning
/// on silent deletion underneath them during an upgrade would be the wrong
/// way round. `limits_configured` reports which state the caller is in.
///
/// Pruning drops the archive only — the memories derived from it are
/// untouched, and [`source_for`] already reads a missing blob as "no source"
/// rather than an error, so a pruned import degrades exactly like one imported
/// before retention was switched on.
pub fn prune(conn: &Connection, dry_run: bool) -> Result<PruneReport, ArchiveError> {
    let max_age_days = env_u64(ARCHIVE_MAX_AGE_DAYS_ENV);
    let max_bytes = env_u64(ARCHIVE_MAX_BYTES_ENV);

    let mut report = PruneReport {
        dry_run,
        limits_configured: max_age_days.is_some() || max_bytes.is_some(),
        ..Default::default()
    };

    let mut stmt = conn.prepare(
        "SELECT import_id, hash, byte_len, archived_at
           FROM import_archives
          ORDER BY archived_at ASC",
    )?;
    let rows: Vec<Retained> = stmt
        .query_map([], |r| {
            let archived_at: String = r.get(3)?;
            Ok(Retained {
                import_id: r.get(0)?,
                hash: r.get(1)?,
                byte_len: r.get::<_, i64>(2)?.max(0) as u64,
                // An unparseable timestamp is treated as epoch, so it sorts
                // oldest and is the first thing an age limit removes. A row
                // whose date cannot be read is exactly the row least worth
                // keeping, and skipping it would make it permanently
                // unprunable.
                archived_at: chrono::DateTime::parse_from_rfc3339(&archived_at)
                    .map(|t| t.with_timezone(&chrono::Utc))
                    .unwrap_or(chrono::DateTime::UNIX_EPOCH),
            })
        })?
        .collect::<SqlResult<_>>()?;
    drop(stmt);

    report.examined = rows.len();
    if !report.limits_configured {
        report.bytes_remaining = distinct_bytes(rows.iter());
        return Ok(report);
    }

    let now = chrono::Utc::now();
    let mut doomed: Vec<&Retained> = Vec::new();
    let mut keeping: Vec<&Retained> = Vec::new();

    for row in &rows {
        let too_old =
            max_age_days.is_some_and(|days| (now - row.archived_at).num_days() > days as i64);
        if too_old {
            doomed.push(row);
        } else {
            keeping.push(row);
        }
    }
    report.removed_for_age = doomed.len();

    if let Some(ceiling) = max_bytes {
        // `keeping` is oldest-first, so advancing the window start evicts in
        // the right order. Recomputed per step rather than subtracted: a blob
        // shared by two retained imports only stops costing bytes when the
        // *last* of them goes, and running-total arithmetic gets that wrong.
        let mut cut = 0;
        while cut < keeping.len() && distinct_bytes(keeping[cut..].iter().copied()) > ceiling {
            doomed.push(keeping[cut]);
            report.removed_for_size += 1;
            cut += 1;
        }
        keeping.drain(..cut);
    }

    report.bytes_remaining = distinct_bytes(keeping.iter().copied());
    report.bytes_reclaimed = distinct_bytes(rows.iter()) - report.bytes_remaining;

    if dry_run {
        return Ok(report);
    }

    for row in doomed {
        forget_import(conn, &row.import_id)?;
    }
    Ok(report)
}

/// Total bytes on disk for a set of retained imports, counting each distinct
/// blob once.
fn distinct_bytes<'a>(rows: impl Iterator<Item = &'a Retained>) -> u64 {
    let mut seen = std::collections::HashSet::new();
    rows.filter(|r| seen.insert(r.hash.clone()))
        .map(|r| r.byte_len)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_path_shards_on_the_first_two_hash_characters() {
        let path = blob_path(Path::new("/archive"), "ab12cd34");
        assert_eq!(path, Path::new("/archive/ab/ab12cd34"));
    }

    #[test]
    fn a_short_hash_still_yields_a_path_rather_than_panicking() {
        // `hash_bytes` is 16 characters, so this is defensive rather than
        // reachable -- but `get(..2)` on a one-character hash returning None
        // would panic under indexing, and a panic in the write path would
        // fail an import that archiving is only supposed to decorate.
        let path = blob_path(Path::new("/archive"), "a");
        assert_eq!(path, Path::new("/archive/__/a"));
    }
}
