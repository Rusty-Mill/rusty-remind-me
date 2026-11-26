//! Shadow write of captured dialogs into the dialog store (#528, D82).
//!
//! The `memories` row stays the record. When `REMIND_ME_DIALOG_SHADOW=1`, each
//! dialog row a capture writes (the first and every continuation) is also
//! appended to `<database file name>.dialogs/` beside the database file (one
//! store per database, so two databases in one directory never mix), so the store can be
//! checked against production traffic before anything switches to it. A
//! failure never fails the capture: it is logged and counted.

use crate::blobs::BlobStore;
use crate::db::dialogs::{DialogLog, NewDialog};
use crate::db::sqlite::SqliteDialogs;
use crate::db::Store;
use std::sync::atomic::{AtomicU64, Ordering};

/// Set to `1` to turn the shadow write on. Off by default.
pub const ENV: &str = "REMIND_ME_DIALOG_SHADOW";

static FAILURES: AtomicU64 = AtomicU64::new(0);

/// Shadow writes that failed in this process.
pub fn failures() -> u64 {
    FAILURES.load(Ordering::Relaxed)
}

fn enabled() -> bool {
    std::env::var(ENV).is_ok_and(|v| v == "1")
}

/// The shadow store's directory for the database file `db`: its own name
/// with `.dialogs` appended, so every database has its own store.
fn dir_for(db: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut name = db.file_name()?.to_os_string();
    name.push(".dialogs");
    Some(db.with_file_name(name))
}

/// Append a committed dialog row to the shadow store, if enabled.
pub fn record_memory(store: &Store<'_>, row: &crate::models::Memory) {
    let Some(capture_id) = row.capture_id.as_deref() else {
        return;
    };
    record(
        store,
        &NewDialog {
            id: &row.id,
            capture_id,
            session_id: row.session_id.as_deref(),
            project: row.project.as_deref(),
            created_at: &row.created_at,
            body: &row.content,
        },
    );
}

/// Append `dialog` to the dialog store beside `store`'s database, if enabled.
/// An in-memory database has no directory, so it is skipped.
pub fn record(store: &Store<'_>, dialog: &NewDialog<'_>) {
    if !enabled() {
        return;
    }
    let Some(dir) = store.path().and_then(dir_for) else {
        return;
    };
    let result = SqliteDialogs::open(&dir)
        .and_then(|s| DialogLog::new(s, BlobStore::new(dir.join("blobs"))).record(dialog));
    if let Err(e) = result {
        FAILURES.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "rusty-remind-me: dialog shadow write of {} failed: {e}",
            dialog.id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::dialogs::DialogStore;

    fn dialog<'a>(id: &'a str, body: &'a str) -> NewDialog<'a> {
        NewDialog {
            id,
            capture_id: "cap_1",
            session_id: None,
            project: None,
            created_at: "2026-11-26T00:00:00Z",
            body,
        }
    }

    #[test]
    fn a_record_lands_in_the_store_and_a_rerun_is_a_duplicate() {
        let dir = std::env::temp_dir().join(format!("shadow_{}", uuid::Uuid::new_v4().simple()));
        let mut log = DialogLog::new(
            SqliteDialogs::open(&dir).unwrap(),
            BlobStore::new(dir.join("blobs")),
        );
        log.record(&dialog("mem_a", "hello")).unwrap();
        log.record(&dialog("mem_a", "hello")).unwrap();
        assert_eq!(log.store().count().unwrap(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
