//! On-demand backups of the node's store.
//!
//! A backup is a copy of the engine directory (`memory.engine`), taken while
//! the tables are held so no write lands half-copied, into a
//! `backups/{label}-{timestamp}.engine` directory beside the database file.
//! Restoring is putting that directory back as `memory.engine`.
//!
//! Backups taken while the node stored in SQLite (`backups/*.db` files,
//! before ADR-0025) still list and are pruned in turn.
//!
//! When the node has a dialog store (#528), each backup also writes its own
//! package `backups/{label}-{timestamp}.dialogs` ([`crate::dialog_backup`],
//! D78), with the same label and retention. In this phase the engine copy
//! is the record, so a failed dialog package is reported in the outcome and
//! never fails the backup. The cloud upload covers the engine copy only.

use crate::db::Store;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Backups retained in the backup directory; older ones are pruned after each
/// new backup. Matches the reference's `REMIND_ME_BACKUP_RETENTION_COUNT`
/// default.
pub const BACKUP_RETENTION_COUNT: usize = 10;

/// Directory name created beside the database file.
const BACKUP_DIR_NAME: &str = "backups";
/// The extension of a backup of the engine store: a directory holding a
/// copy of the engine directory's files.
const ENGINE_BACKUP_EXTENSION: &str = "engine";

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("this database is in memory and has no on-disk location to back up beside")]
    InMemory,
    #[error(transparent)]
    Store(#[from] crate::db::StoreError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

type Result<T> = std::result::Result<T, BackupError>;

/// A backup on disk.
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
    /// What the dialog store's package did. `NoStore` (and left out of the
    /// output) when the node has no dialog store.
    #[serde(default, skip_serializing_if = "DialogPackage::is_no_store")]
    pub dialogs: DialogPackage,
}

/// The dialog store's part of a backup run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DialogPackage {
    /// The node has no dialog store, so there is nothing to package.
    #[default]
    NoStore,
    Written {
        path: String,
        dialogs: u64,
    },
    /// The package could not be built; the engine backup still stands.
    Failed {
        error: String,
    },
}

impl DialogPackage {
    fn is_no_store(&self) -> bool {
        matches!(self, Self::NoStore)
    }
}

fn is_not_configured(outcome: &crate::cloud_backup::UploadOutcome) -> bool {
    matches!(outcome, crate::cloud_backup::UploadOutcome::NotConfigured)
}

/// Microsecond precision, so two backups taken in the same second do not
/// collide on filename.
fn timestamp() -> String {
    Utc::now().format("%Y%m%dT%H%M%S%6fZ").to_string()
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> BackupError {
    let path = path.to_path_buf();
    move |source| BackupError::Io { path, source }
}

/// The `backups/` directory beside the database file.
pub fn backup_dir(store: &Store<'_>) -> Result<PathBuf> {
    let db_path = store.path().ok_or(BackupError::InMemory)?;
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent.join(BACKUP_DIR_NAME))
}

/// List existing backups of the engine, newest first.
pub fn list_backups(dir: &Path) -> Result<Vec<BackupInfo>> {
    list_where(dir, |path, extension| {
        extension == Some("db") || (extension == Some(ENGINE_BACKUP_EXTENSION) && path.is_dir())
    })
}

/// List existing dialog packages, newest first.
pub fn list_dialog_packages(dir: &Path) -> Result<Vec<BackupInfo>> {
    list_where(dir, |path, extension| {
        extension == Some(crate::dialog_backup::EXTENSION) && path.is_dir()
    })
}

/// Entries of `dir` that `keep` accepts (given the path and its extension),
/// newest first. A directory's size is the bytes of every file under it.
fn list_where(dir: &Path, keep: impl Fn(&Path, Option<&str>) -> bool) -> Result<Vec<BackupInfo>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(dir).map_err(io(dir))?;

    let mut backups: Vec<(std::time::SystemTime, BackupInfo)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io(dir))?;
        let path = entry.path();
        if !keep(&path, path.extension().and_then(|e| e.to_str())) {
            continue;
        }
        let metadata = entry.metadata().map_err(io(&path))?;
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        let size_bytes = if path.is_dir() {
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
    let mut total = 0;
    for entry in std::fs::read_dir(dir).map_err(io(dir))? {
        let entry = entry.map_err(io(dir))?;
        let path = entry.path();
        total += if path.is_dir() {
            size_of_tree(&path)?
        } else {
            entry.metadata().map_err(io(dir))?.len()
        };
    }
    Ok(total)
}

/// Delete the entries of `backups` (newest first) beyond `keep`. Returns how
/// many were removed.
fn prune_old_backups(backups: &[BackupInfo], keep: usize) -> usize {
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
    removed
}

/// Package the dialog store beside `store`'s database as
/// `{dir}/{stem}.dialogs`, if there is one. Never an error: a failure is
/// logged and reported in the outcome (see the module docs).
fn package_dialogs(store: &Store<'_>, dir: &Path, stem: &str) -> DialogPackage {
    let Some(source) = store.path().and_then(crate::db::dialogs::dir_beside) else {
        return DialogPackage::NoStore;
    };
    if !source.join(crate::db::sqlite::DIALOGS_FILE).is_file() {
        return DialogPackage::NoStore;
    }
    let dest = dir.join(format!("{stem}.{}", crate::dialog_backup::EXTENSION));
    match crate::dialog_backup::create(&source, &dest) {
        Ok(dialogs) => DialogPackage::Written {
            path: dest.to_string_lossy().to_string(),
            dialogs,
        },
        Err(e) => {
            eprintln!("rusty-remind-me: dialog backup package failed: {e}");
            DialogPackage::Failed {
                error: e.to_string(),
            }
        }
    }
}

/// Back up the store into `backups/{label}-{timestamp}.engine` beside the
/// database file, then prune backups beyond [`BACKUP_RETENTION_COUNT`].
///
/// The engine tables are held for the whole copy, so any page open on
/// another thread finishes first and no write lands half-copied. The copy
/// goes into a `.partial` directory renamed into place when complete, so a
/// crash never leaves something that lists as a backup but is not one.
///
/// There is deliberately **no caller-supplied destination**: the reference's
/// tool takes no parameters, and accepting an arbitrary path would hand callers
/// a write primitive pointed anywhere on disk.
pub fn create_backup(store: &Store<'_>, label: &str) -> Result<BackupOutcome> {
    let dir = backup_dir(store)?;
    std::fs::create_dir_all(&dir).map_err(io(&dir))?;
    let stem = format!("{}-{}", safe_label(label), timestamp());
    let dest = dir.join(format!("{stem}.{ENGINE_BACKUP_EXTENSION}"));
    let partial = dest.with_extension("partial");
    if partial.exists() {
        std::fs::remove_dir_all(&partial).map_err(io(&partial))?;
    }
    store.backend().lock()?.backup_to(&partial)?;
    std::fs::rename(&partial, &dest).map_err(io(&dest))?;
    let dialogs = package_dialogs(store, &dir, &stem);
    // Strictly after the local backup is finished and on disk. A refused,
    // failed or unconfigured upload is reported alongside the backup, never
    // instead of it — the local copy is the one that has to survive.
    let upload = crate::cloud_backup::upload_backup_dir(&dest);
    let pruned = prune_old_backups(&list_backups(&dir)?, BACKUP_RETENTION_COUNT);
    // Dialog packages are pruned on their own count, not reported as
    // `pruned`, which keeps meaning engine backups.
    prune_old_backups(&list_dialog_packages(&dir)?, BACKUP_RETENTION_COUNT);
    Ok(BackupOutcome {
        path: dest.to_string_lossy().to_string(),
        total_backups: list_backups(&dir)?.len(),
        pruned,
        upload,
        dialogs,
    })
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
