//! Reconcile the dialog store against `memories` (#528, D132). Read only.
//!
//! In the shadow phase (D82) the `memories` dialog rows are the record and
//! the dialog store a copy, so the check asks three things of the store:
//!
//! - integrity: every row's body blob exists with the size the row records
//!   ([`Depth::Full`] also re-hashes it);
//! - drift: every row has a live `memories` dialog row with the same id and
//!   the same [`DialogRecord::canonical_hash`] (D77);
//! - not yet migrated: how many `memories` dialog rows the store lacks. A
//!   count, never a finding, until the migration (D3) runs.
//!
//! Nothing is repaired, appended or deleted (D80). On node open the quick
//! pass runs once on a background thread ([`start_for`]) and logs one
//! summary line; `rusty-remind-me dialogs check [--full]` prints the ids.

use crate::blobs::{content_key, BlobStore};
use crate::db::dialogs::{dir_beside, DialogRecord, DialogStore};
use crate::db::memories::Memories;
use crate::db::sqlite::{SqliteDialogs, DIALOGS_FILE};
use crate::db::{Result, Store, StoreError};
use crate::models::{Memory, DIALOG_CATEGORY};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// How hard [`check`] looks at blobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// Existence and size only: no body is read.
    Quick,
    /// Every body read and re-hashed, one at a time.
    Full,
}

/// What [`check`] found. Each list holds dialog ids.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Rows in the dialog store.
    pub rows: u64,
    pub blob_missing: Vec<String>,
    /// Wrong size, or (full check) wrong hash.
    pub blob_mismatch: Vec<String>,
    /// Rows with no live `memories` dialog row of the same id.
    pub no_memory: Vec<String>,
    /// Rows whose `memories` dialog row has a different canonical hash.
    pub drift: Vec<String>,
    /// Live `memories` dialog rows the store does not hold yet.
    pub not_migrated: u64,
}

impl Report {
    /// Problems found; `not_migrated` is not one.
    pub fn findings(&self) -> usize {
        self.blob_missing.len() + self.blob_mismatch.len() + self.no_memory.len() + self.drift.len()
    }
}

static RUNS: AtomicU64 = AtomicU64::new(0);
static LAST_FINDINGS: AtomicU64 = AtomicU64::new(0);

/// Completed checks in this process.
pub fn runs() -> u64 {
    RUNS.load(Ordering::Relaxed)
}

/// Findings of the last completed check in this process (0 before any).
pub fn last_findings() -> u64 {
    LAST_FINDINGS.load(Ordering::Relaxed)
}

/// The dialog file beside `store`'s database, if there is one.
pub(crate) fn store_file(store: &Store<'_>) -> Option<PathBuf> {
    let file = dir_beside(store.path()?)?.join(DIALOGS_FILE);
    file.is_file().then_some(file)
}

/// The row a `memories` dialog row would be in the store: what the shadow
/// write appends (`dialog_shadow::record_memory`). A row with no
/// `capture_id` takes its generated one (D148). The head is not part of the
/// canonical hash, so it is left empty.
pub(crate) fn as_record(m: &Memory) -> DialogRecord {
    DialogRecord {
        id: m.id.clone(),
        capture_id: m
            .capture_id
            .clone()
            .or_else(|| minted_capture_id(m))
            .unwrap_or_default(),
        session_id: m.session_id.clone(),
        project: m.project.clone(),
        created_at: m.created_at.clone(),
        head: String::new(),
        body_hash: content_key(m.content.as_bytes()),
        body_bytes: m.content.len() as u64,
    }
}

/// What a generated `capture_id` groups a row by (D148), in priority order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MintKey {
    /// The capture source and its session id: one id per session.
    Session,
    /// The imported document: one id per import file.
    Doc,
    /// The import recorded in metadata.
    Import,
    /// Nothing to group by: one id for the row alone.
    Row,
}

impl MintKey {
    /// The key's name in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Doc => "doc",
            Self::Import => "import",
            Self::Row => "row",
        }
    }
}

/// Prefix of a generated `capture_id`. Real ids are `cap_` and 32 hex
/// digits, so `gen_` cannot collide, and marks the id as generated.
pub const MINTED_PREFIX: &str = "cap_gen_";

/// The group a row with no `capture_id` falls in, and its key string.
pub(crate) fn mint_group(m: &Memory) -> (MintKey, String) {
    let present = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(str::to_owned);
    if let Some(session) = present(&m.session_id) {
        return (
            MintKey::Session,
            group_key(&["session", &m.source, &session]),
        );
    }
    if let Some(doc) = present(&m.doc_id) {
        return (MintKey::Doc, group_key(&["doc", &doc]));
    }
    let import = m
        .metadata
        .get("import_id")
        .and_then(serde_json::Value::as_str);
    if let Some(import) = import.filter(|s| !s.is_empty()) {
        return (MintKey::Import, group_key(&["import", import]));
    }
    (MintKey::Row, group_key(&["row", &m.id]))
}

/// The parts of a group key as netstrings (`<byte length>:<part>,`), so
/// no stored value, separators included, can make two groups one.
pub(crate) fn group_key(parts: &[&str]) -> String {
    parts.iter().map(|p| format!("{}:{p},", p.len())).collect()
}

/// The generated `capture_id` of a row that has none (D147, D148, D150): the same
/// on every run, so the dry run's target digest binds the apply. `None`
/// when the row has its own.
pub(crate) fn minted_capture_id(m: &Memory) -> Option<String> {
    if m.capture_id.is_some() {
        return None;
    }
    let (_, key) = mint_group(m);
    Some(format!(
        "{MINTED_PREFIX}{}",
        &content_key(key.as_bytes())[..32]
    ))
}

/// The state of a row's blob.
pub(crate) enum Blob {
    Intact,
    Missing,
    Mismatch,
}

/// The state of `r`'s blob at `depth`.
pub(crate) fn blob_state(blobs: &BlobStore, r: &DialogRecord, depth: Depth) -> Result<Blob> {
    let io = |e: std::io::Error| StoreError::Engine(format!("dialog blob {}: {e}", r.body_hash));
    match depth {
        Depth::Quick => match std::fs::metadata(blobs.path(&r.body_hash)) {
            Ok(meta) if meta.len() == r.body_bytes => Ok(Blob::Intact),
            Ok(_) => Ok(Blob::Mismatch),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Blob::Missing),
            Err(e) => Err(io(e)),
        },
        Depth::Full => match blobs.get(&r.body_hash).map_err(io)? {
            // The size too: the canonical hash leaves it out, so nothing
            // else would catch a wrong recorded size.
            Some(bytes)
                if bytes.len() as u64 == r.body_bytes && content_key(&bytes) == r.body_hash =>
            {
                Ok(Blob::Intact)
            }
            Some(_) => Ok(Blob::Mismatch),
            None => Ok(Blob::Missing),
        },
    }
}

/// Check the dialog store beside `store`'s database. `None` when there is
/// no dialog store.
///
/// # Errors
///
/// [`StoreError`] if either store cannot be read.
pub fn check(store: &Store<'_>, depth: Depth) -> Result<Option<Report>> {
    let Some(file) = store_file(store) else {
        return Ok(None);
    };
    let dialogs = SqliteDialogs::open_read_only(&file)?;
    let blobs = BlobStore::new(file.with_file_name("blobs"));
    let memories = Memories::new(store);
    let live: HashMap<String, Memory> = memories
        .exportable(false, Some(DIALOG_CATEGORY), &[])?
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();
    let report = reconcile(&memories, &dialogs, &blobs, depth, live)?;
    RUNS.fetch_add(1, Ordering::Relaxed);
    LAST_FINDINGS.store(report.findings() as u64, Ordering::Relaxed);
    Ok(Some(report))
}

/// Scan `dialogs` against `live`, the `memories` dialog rows read before
/// the scan.
fn reconcile(
    memories: &Memories<'_>,
    dialogs: &SqliteDialogs,
    blobs: &BlobStore,
    depth: Depth,
    mut live: HashMap<String, Memory>,
) -> Result<Report> {
    let mut report = Report::default();
    dialogs.scan(&mut |r| {
        report.rows += 1;
        match blob_state(blobs, &r, depth)? {
            Blob::Missing => report.blob_missing.push(r.id.clone()),
            Blob::Mismatch => report.blob_mismatch.push(r.id.clone()),
            Blob::Intact => {}
        }
        // A capture may commit between the `memories` read and the scan,
        // so an id missing from the read is looked up again before it is a
        // finding.
        let found = match live.remove(&r.id) {
            Some(m) => Some(m),
            None => memories
                .get_live(&r.id)?
                .filter(|m| m.category == DIALOG_CATEGORY),
        };
        match found {
            None => report.no_memory.push(r.id),
            Some(m) if as_record(&m).canonical_hash() != r.canonical_hash() => {
                report.drift.push(r.id);
            }
            Some(_) => {}
        }
        Ok(())
    })?;
    report.not_migrated = live.len() as u64;
    Ok(report)
}

/// One log line for `report`.
fn summary(path: &Path, report: &Report, millis: u128) -> String {
    format!(
        "rusty-remind-me: dialog store {}: {} rows, {} findings \
         (blob missing {}, blob mismatch {}, no memory {}, drift {}), \
         {} not yet migrated, checked in {millis} ms",
        path.display(),
        report.rows,
        report.findings(),
        report.blob_missing.len(),
        report.blob_mismatch.len(),
        report.no_memory.len(),
        report.drift.len(),
        report.not_migrated,
    )
}

/// Run the quick check once on a background thread and log its summary.
/// `None` for a database with no dialog store, or if the thread cannot be
/// spawned (reported). Never blocks the caller.
pub fn start_for(store: &Store<'_>) -> Option<std::thread::JoinHandle<()>> {
    let file = store_file(store)?;
    let source = store.secondary_source()?;
    let spawned = std::thread::Builder::new()
        .name("dialog-reconcile".to_string())
        .spawn(move || {
            let started = std::time::Instant::now();
            match check(&source.store(), Depth::Quick) {
                Ok(Some(report)) => {
                    eprintln!("{}", summary(&file, &report, started.elapsed().as_millis()));
                }
                Ok(None) => {}
                Err(e) => eprintln!("rusty-remind-me: dialog store check failed: {e}"),
            }
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            eprintln!("rusty-remind-me: dialog store check not started: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::dialogs::{DialogLog, NewDialog};

    #[test]
    fn a_row_committed_after_the_memories_read_is_not_an_orphan() {
        let dir = std::env::temp_dir().join(format!("reconcile_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::Database::open(dir.join("remind_me.db")).unwrap();
        let input = crate::AutoCaptureInput {
            conversation: "user: hi\nassistant: hello".into(),
            summary: "A greeting.".into(),
            title: String::new(),
            tags: vec![],
            category: "conversation".into(),
            metadata: serde_json::json!({}),
            project: None,
        };
        let made = crate::capture::auto_capture(&db.store(), &input).unwrap();
        let row = Memories::new(&db.store())
            .get_live(&made.dialog_id)
            .unwrap()
            .unwrap();
        let store_dir = dir.join("dialogs");
        let blobs = BlobStore::new(store_dir.join("blobs"));
        let mut log = DialogLog::new(SqliteDialogs::open(&store_dir).unwrap(), blobs.clone());
        log.record(&NewDialog {
            id: &row.id,
            capture_id: &made.capture_id,
            session_id: row.session_id.as_deref(),
            project: row.project.as_deref(),
            created_at: &row.created_at,
            body: &row.content,
        })
        .unwrap();

        // An empty read stands for one taken before the capture committed.
        let report = reconcile(
            &Memories::new(&db.store()),
            log.store(),
            &blobs,
            Depth::Quick,
            HashMap::new(),
        )
        .unwrap();
        assert_eq!(report.findings(), 0, "{report:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
