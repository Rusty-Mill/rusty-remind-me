//! The dialog store's own backup package (#528, D78).
//!
//! A package is a directory `backups/<label>-<timestamp>.dialogs` holding
//!
//! - `dialogs.sqlite3`: a consistent copy of the store (`VACUUM INTO`),
//! - `blobs/`: the body of every row in that copy, in the blob layout,
//! - `manifest.jsonl`: one line per row with its canonical hash (D77) and
//!   body hash.
//!
//! It is built in `<name>.dialogs.partial` and renamed when complete, so a
//! crash never leaves something that lists as a package but is not one.
//! Only blobs the copy's rows name are copied: a body is published before
//! its row (ADR-0037), so every row in the copy has its blob, and an orphan
//! blob from a crashed write is not part of the record.
//!
//! Restore puts a verified package back as a new store directory and checks
//! the result against the manifest again (G8's way). It never writes over
//! an existing store.

use crate::blobs::{content_key, BlobStore};
use crate::db::dialogs::{DialogRecord, DialogStore};
use crate::db::sqlite::{SqliteDialogs, DIALOGS_FILE};
use crate::db::{Result, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};

/// The extension of a dialog backup package.
pub const EXTENSION: &str = "dialogs";
const MANIFEST: &str = "manifest.jsonl";
const BLOBS: &str = "blobs";

/// One manifest line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ManifestRow {
    id: String,
    row_hash: String,
    body_hash: String,
}

impl ManifestRow {
    fn of(r: &DialogRecord) -> Self {
        Self {
            id: r.id.clone(),
            row_hash: r.canonical_hash(),
            body_hash: r.body_hash.clone(),
        }
    }
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |e| StoreError::Engine(format!("dialog backup: {}: {e}", path.display()))
}

/// `path` with `.partial` appended to its whole name.
fn partial_of(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".partial");
    PathBuf::from(name)
}

/// The bytes of `hash` from `blobs`, checked against the full hash.
fn verified_blob(blobs: &BlobStore, id: &str, hash: &str) -> Result<Vec<u8>> {
    let bytes = blobs
        .get(hash)
        .map_err(io_error(blobs.root()))?
        .ok_or_else(|| StoreError::Invalid(format!("dialog {id}: blob {hash} is missing")))?;
    if content_key(&bytes) != hash {
        return Err(StoreError::Invalid(format!(
            "dialog {id}: blob {hash} does not match its hash"
        )));
    }
    Ok(bytes)
}

/// Build a package of the store in `store_dir` at `dest`. The number of
/// dialogs packaged.
///
/// # Errors
///
/// [`StoreError`] if `dest` exists, the store cannot be read, a row's blob
/// is missing or corrupt, or a file cannot be written. Nothing is left at
/// `dest` on error.
pub fn create(store_dir: &Path, dest: &Path) -> Result<u64> {
    if dest.exists() {
        return Err(StoreError::Invalid(format!(
            "dialog backup: {} already exists",
            dest.display()
        )));
    }
    let partial = partial_of(dest);
    if partial.exists() {
        std::fs::remove_dir_all(&partial).map_err(io_error(&partial))?;
    }
    std::fs::create_dir_all(&partial).map_err(io_error(&partial))?;
    let rows = fill(store_dir, &partial).inspect_err(|_| {
        // Best effort: a leftover `.partial` never lists as a package and
        // is cleared by the next run, so the original error is the one kept.
        let _ = std::fs::remove_dir_all(&partial);
    })?;
    std::fs::rename(&partial, dest).map_err(io_error(dest))?;
    Ok(rows)
}

/// Snapshot, copy blobs and write the manifest into `partial`.
fn fill(store_dir: &Path, partial: &Path) -> Result<u64> {
    let snapshot = partial.join(DIALOGS_FILE);
    SqliteDialogs::open(store_dir)?.snapshot_into(&snapshot)?;
    let copy = SqliteDialogs::open_read_only(&snapshot)?;
    let from = BlobStore::new(store_dir.join(BLOBS));
    let to = BlobStore::new(partial.join(BLOBS));
    let manifest_path = partial.join(MANIFEST);
    let file = std::fs::File::create(&manifest_path).map_err(io_error(&manifest_path))?;
    let mut manifest = BufWriter::new(file);
    let mut rows = 0;
    copy.scan(&mut |r| {
        let bytes = verified_blob(&from, &r.id, &r.body_hash)?;
        to.put_durable(&bytes).map_err(io_error(to.root()))?;
        let line = serde_json::to_string(&ManifestRow::of(&r))
            .map_err(|e| StoreError::Engine(format!("dialog backup manifest: {e}")))?;
        writeln!(manifest, "{line}").map_err(io_error(&manifest_path))?;
        rows += 1;
        Ok(())
    })?;
    manifest
        .into_inner()
        .map_err(|e| io_error(&manifest_path)(e.into_error()))?
        .sync_all()
        .map_err(io_error(&manifest_path))?;
    Ok(rows)
}

fn read_manifest(package: &Path) -> Result<HashMap<String, ManifestRow>> {
    let path = package.join(MANIFEST);
    let file = std::fs::File::open(&path).map_err(io_error(&path))?;
    let mut rows = HashMap::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line.map_err(io_error(&path))?;
        let row: ManifestRow = serde_json::from_str(&line)
            .map_err(|e| StoreError::Invalid(format!("{}: {e}", path.display())))?;
        if let Some(dup) = rows.insert(row.id.clone(), row) {
            return Err(StoreError::Invalid(format!(
                "{}: dialog {} is listed twice",
                path.display(),
                dup.id
            )));
        }
    }
    Ok(rows)
}

/// Check the store files in `dir` against `manifest`: the same rows, each
/// with its canonical hash and an intact blob. The row count.
fn check(dir: &Path, manifest: &HashMap<String, ManifestRow>) -> Result<u64> {
    let store = SqliteDialogs::open_read_only(&dir.join(DIALOGS_FILE))?;
    let blobs = BlobStore::new(dir.join(BLOBS));
    let mut rows = 0_u64;
    store.scan(&mut |r| {
        if manifest.get(&r.id) != Some(&ManifestRow::of(&r)) {
            return Err(StoreError::Invalid(format!(
                "dialog {} does not match the manifest",
                r.id
            )));
        }
        verified_blob(&blobs, &r.id, &r.body_hash)?;
        rows += 1;
        Ok(())
    })?;
    if usize::try_from(rows).ok() != Some(manifest.len()) {
        return Err(StoreError::Invalid(format!(
            "{} rows stored, {} in the manifest",
            rows,
            manifest.len()
        )));
    }
    Ok(rows)
}

/// Check a package: every row matches the manifest and every blob its
/// hash. The number of dialogs.
///
/// # Errors
///
/// [`StoreError::Invalid`] naming the first mismatch, otherwise
/// [`StoreError::Engine`] if the package cannot be read.
pub fn verify(package: &Path) -> Result<u64> {
    check(package, &read_manifest(package)?)
}

/// Restore `package` as a new store at `store_dir`, then check the restored
/// store against the manifest. The number of dialogs restored.
///
/// # Errors
///
/// [`StoreError::Invalid`] if `store_dir` exists or the package fails
/// [`verify`]; otherwise the filesystem's error. Nothing is left at
/// `store_dir` on error.
pub fn restore(package: &Path, store_dir: &Path) -> Result<u64> {
    if store_dir.exists() {
        return Err(StoreError::Invalid(format!(
            "dialog restore: {} already exists",
            store_dir.display()
        )));
    }
    let manifest = read_manifest(package)?;
    check(package, &manifest)?;
    let partial = partial_of(store_dir);
    if partial.exists() {
        std::fs::remove_dir_all(&partial).map_err(io_error(&partial))?;
    }
    std::fs::create_dir_all(&partial).map_err(io_error(&partial))?;
    let restored = put_back(package, &partial, &manifest).inspect_err(|_| {
        // Best effort, as in `create`: the original error is the one kept.
        let _ = std::fs::remove_dir_all(&partial);
    })?;
    std::fs::rename(&partial, store_dir).map_err(io_error(store_dir))?;
    Ok(restored)
}

/// Copy the package's file and blobs into `partial` and check the copy.
fn put_back(
    package: &Path,
    partial: &Path,
    manifest: &HashMap<String, ManifestRow>,
) -> Result<u64> {
    let file = partial.join(DIALOGS_FILE);
    std::fs::copy(package.join(DIALOGS_FILE), &file).map_err(io_error(&file))?;
    std::fs::File::open(&file)
        .and_then(|f| f.sync_all())
        .map_err(io_error(&file))?;
    let from = BlobStore::new(package.join(BLOBS));
    let to = BlobStore::new(partial.join(BLOBS));
    for row in manifest.values() {
        let bytes = verified_blob(&from, &row.id, &row.body_hash)?;
        to.put_durable(&bytes).map_err(io_error(to.root()))?;
    }
    check(partial, manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::dialogs::{DialogLog, NewDialog};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dialog_backup_{name}_{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A store at `<root>/store` with two dialogs (one body shared) and an
    /// orphan blob.
    fn store_with_dialogs(root: &Path) -> PathBuf {
        let dir = root.join("store");
        let mut log = DialogLog::new(
            SqliteDialogs::open(&dir).unwrap(),
            BlobStore::new(dir.join(BLOBS)),
        );
        for (id, body) in [("d1", "hello"), ("d2", "world"), ("d3", "hello")] {
            log.record(&NewDialog {
                id,
                capture_id: "c1",
                session_id: Some("s1"),
                project: None,
                created_at: "2026-11-27T00:00:00Z",
                body,
            })
            .unwrap();
        }
        BlobStore::new(dir.join(BLOBS)).put(b"orphan").unwrap();
        dir
    }

    #[test]
    fn a_package_round_trips_through_restore() {
        let root = scratch("round");
        let store = store_with_dialogs(&root);
        let package = root.join("b.dialogs");
        assert_eq!(create(&store, &package).unwrap(), 3);
        assert!(!partial_of(&package).exists());
        assert_eq!(verify(&package).unwrap(), 3);
        // The orphan blob is not part of the record.
        let orphan = content_key(b"orphan");
        assert!(!BlobStore::new(package.join(BLOBS)).path(&orphan).exists());

        let restored = root.join("restored");
        assert_eq!(restore(&package, &restored).unwrap(), 3);
        let log = DialogLog::new(
            SqliteDialogs::open(&restored).unwrap(),
            BlobStore::new(restored.join(BLOBS)),
        );
        let d2 = log.store().get("d2").unwrap().unwrap();
        assert_eq!(log.body(&d2).unwrap(), "world");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_store_packages_as_zero_rows() {
        let root = scratch("empty");
        let store = root.join("store");
        SqliteDialogs::open(&store).unwrap();
        let package = root.join("b.dialogs");
        assert_eq!(create(&store, &package).unwrap(), 0);
        assert_eq!(verify(&package).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_blob_fails_the_backup_and_leaves_nothing() {
        let root = scratch("missing");
        let store = store_with_dialogs(&root);
        let blobs = BlobStore::new(store.join(BLOBS));
        std::fs::remove_file(blobs.path(&content_key(b"world"))).unwrap();
        let package = root.join("b.dialogs");
        assert!(matches!(
            create(&store, &package),
            Err(StoreError::Invalid(_))
        ));
        assert!(!package.exists());
        assert!(!partial_of(&package).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_existing_destination_is_refused() {
        let root = scratch("exists");
        let store = store_with_dialogs(&root);
        let package = root.join("b.dialogs");
        create(&store, &package).unwrap();
        assert!(matches!(
            create(&store, &package),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            restore(&package, &store),
            Err(StoreError::Invalid(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_blob_in_the_package_fails_verify_and_restore() {
        let root = scratch("corrupt");
        let store = store_with_dialogs(&root);
        let package = root.join("b.dialogs");
        create(&store, &package).unwrap();
        let blob = BlobStore::new(package.join(BLOBS)).path(&content_key(b"hello"));
        std::fs::write(&blob, b"tampered").unwrap();
        assert!(matches!(verify(&package), Err(StoreError::Invalid(_))));
        let restored = root.join("restored");
        assert!(matches!(
            restore(&package, &restored),
            Err(StoreError::Invalid(_))
        ));
        assert!(!restored.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_manifest_that_disagrees_with_the_rows_fails_verify() {
        let root = scratch("manifest");
        let store = store_with_dialogs(&root);
        let package = root.join("b.dialogs");
        create(&store, &package).unwrap();
        let path = package.join(MANIFEST);
        let text = std::fs::read_to_string(&path).unwrap();
        // A dropped line: one row more in the file than in the manifest.
        let short: String = text.lines().skip(1).map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, &short).unwrap();
        assert!(matches!(verify(&package), Err(StoreError::Invalid(_))));
        // A changed row hash.
        let altered = text.replacen("\"row_hash\":\"", "\"row_hash\":\"0", 1);
        std::fs::write(&path, altered).unwrap();
        assert!(matches!(verify(&package), Err(StoreError::Invalid(_))));
        // An extra line for a row the file does not hold.
        let extra = format!("{text}{{\"id\":\"ghost\",\"row_hash\":\"x\",\"body_hash\":\"y\"}}\n");
        std::fs::write(&path, extra).unwrap();
        assert!(matches!(verify(&package), Err(StoreError::Invalid(_))));
        let _ = std::fs::remove_dir_all(&root);
    }
}
