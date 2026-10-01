//! On-demand SQLite backups.
//!
//! Uses SQLite's online backup API rather than a file copy. The database runs in
//! WAL mode (`ARCHITECTURE.md` §6), so copying the `.db` file alone would miss
//! anything still in the `-wal` and could capture a torn or partially
//! checkpointed page while a write is in flight.

use crate::db::Store;
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
/// The extension of a backup of a store on the engine: a directory holding
/// the SQLite file and the engine directory.
const ENGINE_BACKUP_EXTENSION: &str = "engine";

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("this database is in memory and has no on-disk location to back up beside")]
    InMemory,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Store(#[from] crate::db::StoreError),
    /// Backups copy the SQLite file, and this store is not one.
    #[error("this store is not SQLite, so it has no database file to back up")]
    NotSqlite,
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
fn database_path(store: &Store<'_>) -> crate::db::Result<Option<PathBuf>> {
    crate::db::database_path(store)
}

/// The `backups/` directory beside the database file.
pub fn backup_dir(store: &Store<'_>) -> Result<PathBuf> {
    let db_path = database_path(store)?.ok_or(BackupError::InMemory)?;
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
        let extension = path.extension().and_then(|e| e.to_str());
        let is_engine = extension == Some(ENGINE_BACKUP_EXTENSION) && path.is_dir();
        if extension != Some("db") && !is_engine {
            continue;
        }
        let metadata = entry.metadata().map_err(|source| BackupError::Io {
            path: path.clone(),
            source,
        })?;
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        let size_bytes = if is_engine {
            size_of_tree(&path)?
        } else {
            metadata.len()
        };
        backups.push((
            modified,
            BackupInfo {
                filename: entry.file_name().to_string_lossy().to_string(),
                path: path.to_string_lossy().to_string(),
                size_bytes,
                created_at: chrono::DateTime::<Utc>::from(modified).to_rfc3339(),
            },
        ));
    }

    // Newest first. Sorted by mtime with the filename as a tiebreaker, because
    // several backups can land inside one filesystem timestamp tick.
    backups.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.filename.cmp(&a.1.filename)));
    Ok(backups.into_iter().map(|(_, info)| info).collect())
}

/// The bytes in every file under `dir`: an engine backup's size.
fn size_of_tree(dir: &Path) -> Result<u64> {
    let io = |source| BackupError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let mut total = 0;
    for entry in std::fs::read_dir(dir).map_err(io)? {
        let entry = entry.map_err(io)?;
        let path = entry.path();
        total += if path.is_dir() {
            size_of_tree(&path)?
        } else {
            entry.metadata().map_err(io)?.len()
        };
    }
    Ok(total)
}

/// Delete backups beyond `keep`, oldest first. Returns how many were removed.
fn prune_old_backups(dir: &Path, keep: usize) -> Result<usize> {
    let backups = list_backups(dir)?;
    let mut removed = 0;
    for stale in backups.iter().skip(keep) {
        // A backup that vanished under us is not an error worth failing the
        // whole call for — the goal state (it is gone) already holds.
        let path = Path::new(&stale.path);
        let removed_one = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        if removed_one.is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Create a WAL-safe online backup of the database.
///
/// Written to `backups/{label}-{timestamp}.db` beside the database file, then
/// older backups beyond [`BACKUP_RETENTION_COUNT`] are pruned. A store on the
/// engine is backed up to a `backups/{label}-{timestamp}.engine` directory
/// instead (see [`back_up_engine`]).
///
/// There is deliberately **no caller-supplied destination**: the reference's
/// tool takes no parameters, and accepting an arbitrary path would hand callers
/// a write primitive pointed anywhere on disk.
pub fn create_backup(store: &Store<'_>, label: &str) -> Result<BackupOutcome> {
    let dir = backup_dir(store)?;
    std::fs::create_dir_all(&dir).map_err(|source| BackupError::Io {
        path: dir.clone(),
        source,
    })?;
    let stem = format!("{}-{}", safe_label(label), timestamp());

    #[cfg(feature = "engine-store")]
    if let Some(tables) = store.engine() {
        let dest_path = dir.join(format!("{stem}.{ENGINE_BACKUP_EXTENSION}"));
        back_up_engine(store, tables, &dest_path)?;
        return finish(
            &dir,
            &dest_path,
            crate::cloud_backup::upload_backup_dir(&dest_path),
        );
    }

    let dest_path = dir.join(format!("{stem}.db"));
    back_up_sqlite(store, &dest_path)?;
    // Strictly after the local backup is finished and on disk. A refused,
    // failed or unconfigured upload is reported alongside the backup, never
    // instead of it — the local copy is the one that has to survive.
    let upload = crate::cloud_backup::upload_backup(&dest_path);
    finish(&dir, &dest_path, upload)
}

/// Keep the label to a filename-safe slug so it cannot introduce path
/// separators or traversal segments.
fn safe_label(label: &str) -> String {
    let slug: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "manual".to_string()
    } else {
        slug.to_string()
    }
}

/// SQLite's online backup of the store's connection into `dest`.
fn back_up_sqlite(store: &Store<'_>, dest: &Path) -> Result<()> {
    let mut dest = Connection::open(dest)?;
    let source = store.sqlite().ok_or(BackupError::NotSqlite)?;
    let backup = Backup::new(source, &mut dest)?;
    backup.run_to_completion(100, Duration::from_millis(50), None)?;
    Ok(())
}

/// Back up a store on the engine into the directory `dest`: the SQLite file
/// under its own name, and the engine directory beside it under its own
/// name, as [`crate::db::Database::open`] expects to find them. Restoring
/// is putting both back.
///
/// The tables are held for the whole copy, so any page open on another
/// thread finishes first and no write lands half-copied. The copy goes into
/// a `.partial` directory renamed into place when complete, so a crash
/// never leaves something that lists as a backup but is not one.
#[cfg(feature = "engine-store")]
fn back_up_engine(
    store: &Store<'_>,
    tables: &crate::db::engine::EngineLock,
    dest: &Path,
) -> Result<()> {
    let db_path = database_path(store)?.ok_or(BackupError::InMemory)?;
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| BackupError::Io { path, source }
    };
    let partial = dest.with_extension("partial");
    if partial.exists() {
        std::fs::remove_dir_all(&partial).map_err(io(&partial))?;
    }
    std::fs::create_dir(&partial).map_err(io(&partial))?;
    let file_name = db_path.file_name().ok_or(BackupError::InMemory)?;
    let engine_dir = crate::db::engine_dir(&db_path);
    let engine_name = engine_dir.file_name().ok_or(BackupError::InMemory)?;
    {
        let held = tables.lock();
        back_up_sqlite(store, &partial.join(file_name))?;
        held.copy_files_to(&partial.join(engine_name))?;
    }
    std::fs::rename(&partial, dest).map_err(io(dest))?;
    Ok(())
}

/// Prune old backups and report the one just written.
fn finish(
    dir: &Path,
    dest_path: &Path,
    upload: crate::cloud_backup::UploadOutcome,
) -> Result<BackupOutcome> {
    let pruned = prune_old_backups(dir, BACKUP_RETENTION_COUNT)?;
    Ok(BackupOutcome {
        path: dest_path.to_string_lossy().to_string(),
        total_backups: list_backups(dir)?.len(),
        pruned,
        upload,
    })
}
