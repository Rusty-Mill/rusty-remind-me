//! On-demand SQLite backups.
//!
//! Uses SQLite's online backup API rather than a file copy. The database runs in
//! WAL mode (`ARCHITECTURE.md` §6), so copying the `.db` file alone would miss
//! anything still in the `-wal` and could capture a torn or partially
//! checkpointed page while a write is in flight.

use chrono::Utc;
use rusqlite::backup::Backup;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Backups retained in the backup directory; older ones are pruned after each
/// new backup. Matches the reference's `REMIND_ME_BACKUP_RETENTION_COUNT`
/// default.
pub const BACKUP_RETENTION_COUNT: usize = 10;

/// Directory name created beside the database file.
const BACKUP_DIR_NAME: &str = "backups";

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("this database is in memory and has no on-disk location to back up beside")]
    InMemory,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

type Result<T> = std::result::Result<T, BackupError>;

/// A backup file on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupInfo {
    pub filename: String,
    pub path: String,
    pub size_bytes: u64,
    pub created_at: String,
}

/// Result of a backup run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupOutcome {
    pub path: String,
    /// Backups retained after pruning, newest first.
    pub total_backups: usize,
    pub pruned: usize,
    /// What the optional cloud upload did. `NotConfigured` on the ordinary
    /// path. Reported rather than swallowed, so a refused upload is visible to
    /// whoever asked for the backup instead of only appearing in a log.
    #[serde(skip_serializing_if = "is_not_configured")]
    pub upload: crate::cloud_backup::UploadOutcome,
}

fn is_not_configured(outcome: &crate::cloud_backup::UploadOutcome) -> bool {
    matches!(outcome, crate::cloud_backup::UploadOutcome::NotConfigured)
}

/// Microsecond precision, so two backups taken in the same second do not
/// collide on filename.
fn timestamp() -> String {
    Utc::now().format("%Y%m%dT%H%M%S%6fZ").to_string()
}

/// Where the main database lives on disk, or `None` for an in-memory database.
fn database_path(conn: &Connection) -> rusqlite::Result<Option<PathBuf>> {
    let path: String = conn.query_row("PRAGMA database_list", [], |row| row.get(2))?;
    Ok(if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    })
}

/// The `backups/` directory beside the database file.
pub fn backup_dir(conn: &Connection) -> Result<PathBuf> {
    let db_path = database_path(conn)?.ok_or(BackupError::InMemory)?;
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent.join(BACKUP_DIR_NAME))
}

/// List existing backups, newest first.
pub fn list_backups(dir: &Path) -> Result<Vec<BackupInfo>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(dir).map_err(|source| BackupError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    let mut backups: Vec<(std::time::SystemTime, BackupInfo)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| BackupError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("db") {
            continue;
        }
        let metadata = entry.metadata().map_err(|source| BackupError::Io {
            path: path.clone(),
            source,
        })?;
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        backups.push((
            modified,
            BackupInfo {
                filename: entry.file_name().to_string_lossy().to_string(),
                path: path.to_string_lossy().to_string(),
                size_bytes: metadata.len(),
                created_at: chrono::DateTime::<Utc>::from(modified).to_rfc3339(),
            },
        ));
    }

    // Newest first. Sorted by mtime with the filename as a tiebreaker, because
    // several backups can land inside one filesystem timestamp tick.
    backups.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.filename.cmp(&a.1.filename)));
    Ok(backups.into_iter().map(|(_, info)| info).collect())
}

/// Delete backups beyond `keep`, oldest first. Returns how many were removed.
fn prune_old_backups(dir: &Path, keep: usize) -> Result<usize> {
    let backups = list_backups(dir)?;
    let mut removed = 0;
    for stale in backups.iter().skip(keep) {
        // A backup that vanished under us is not an error worth failing the
        // whole call for — the goal state (it is gone) already holds.
        if std::fs::remove_file(&stale.path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Create a WAL-safe online backup of the database.
///
/// Written to `backups/{label}-{timestamp}.db` beside the database file, then
/// older backups beyond [`BACKUP_RETENTION_COUNT`] are pruned.
///
/// There is deliberately **no caller-supplied destination**: the reference's
/// tool takes no parameters, and accepting an arbitrary path would hand callers
/// a write primitive pointed anywhere on disk.
pub fn create_backup(conn: &Connection, label: &str) -> Result<BackupOutcome> {
    let dir = backup_dir(conn)?;
    std::fs::create_dir_all(&dir).map_err(|source| BackupError::Io {
        path: dir.clone(),
        source,
    })?;

    // Keep the label to a filename-safe slug so it cannot introduce path
    // separators or traversal segments.
    let safe_label: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let safe_label = safe_label.trim_matches('-').to_string();
    let safe_label = if safe_label.is_empty() {
        "manual".to_string()
    } else {
        safe_label
    };

    let dest_path = dir.join(format!("{}-{}.db", safe_label, timestamp()));

    {
        let mut dest = Connection::open(&dest_path)?;
        let backup = Backup::new(conn, &mut dest)?;
        backup.run_to_completion(100, Duration::from_millis(50), None)?;
    }

    let pruned = prune_old_backups(&dir, BACKUP_RETENTION_COUNT)?;

    // Strictly after the local backup is finished and on disk. A refused,
    // failed or unconfigured upload is reported alongside the backup, never
    // instead of it — the local copy is the one that has to survive.
    let upload = crate::cloud_backup::upload_backup(&dest_path);

    Ok(BackupOutcome {
        path: dest_path.to_string_lossy().to_string(),
        total_backups: list_backups(&dir)?.len(),
        pruned,
        upload,
    })
}
