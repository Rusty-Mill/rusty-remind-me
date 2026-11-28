//! Dry run of the dialog backfill (#528, D144, D145). Read only.
//!
//! Plans the copy of every live `memories` dialog row into the dialog store
//! and reports what it would do, writing nothing: no store, file, blob or
//! row is created. The apply waits for Nano's go on these counts (D3).
//!
//! A row with no `capture_id` moves under a generated one, one per
//! conversation (D147, D148); the report counts them by grouping key. A
//! store row whose id has a different canonical hash
//! (D77) is drift: the apply would refuse it. The two digests let the
//! later apply prove it reached exactly the store Nano approved.

use crate::blobs::{content_key, BlobStore};
use crate::db::dialogs::DialogStore;
use crate::db::memories::Memories;
use crate::db::sqlite::SqliteDialogs;
use crate::db::{Result, Store};
use crate::dialog_reconcile::{
    as_record, blob_state, mint_group, minted_capture_id, store_file, Blob, Depth,
};
use crate::models::{Memory, DIALOG_CATEGORY};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// What a backfill would do. Lists hold dialog ids.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Live `memories` dialog rows.
    pub live: u64,
    /// Of `live`, how many are marked sensitive.
    pub sensitive: u64,
    /// Rows the apply would append, and their body bytes.
    pub would_append: u64,
    pub would_append_bytes: u64,
    /// Rows the store already holds with the same canonical hash.
    pub already_present: u64,
    /// Rows the store holds under the same id with a different hash.
    pub drift: Vec<String>,
    /// Rows with no `capture_id`, which move under a generated one (D147),
    /// and how many distinct generated ids they make.
    pub minted_rows: u64,
    pub minted_capture_ids: u64,
    /// The same, by grouping key (`session`, `doc`, `import`, `row`): `row`
    /// counts rows that fell back to an id of their own (D148).
    pub minted_by_key: BTreeMap<String, MintCount>,
    /// Store rows whose blob is missing or the wrong size.
    pub blob_missing: Vec<String>,
    pub blob_mismatch: Vec<String>,
    /// Store rows with no live `memories` dialog row.
    pub no_memory: Vec<String>,
    /// Rows in the store now.
    pub store_rows: u64,
    /// sha256 over the sorted canonical hashes of the store now.
    pub store_digest: String,
    /// Rows, and the same digest, the store would hold after the apply.
    pub target_rows: u64,
    pub target_digest: String,
}

/// Generated capture ids under one grouping key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintCount {
    /// Distinct generated ids.
    pub groups: u64,
    pub rows: u64,
}

impl Report {
    /// Problems that would stop the apply; rows to append and rows with a
    /// generated capture id are not.
    pub fn findings(&self) -> usize {
        self.drift.len() + self.blob_missing.len() + self.blob_mismatch.len() + self.no_memory.len()
    }
}

/// Plan the backfill of `store`'s dialog rows, writing nothing.
///
/// # Errors
///
/// [`crate::db::StoreError`] if either store cannot be read.
pub fn plan(store: &Store<'_>) -> Result<Report> {
    let memories = Memories::new(store);
    let rows = memories.exportable(false, Some(DIALOG_CATEGORY), &[])?;
    let mut report = Report {
        live: rows.len() as u64,
        sensitive: rows.iter().filter(|m| m.sensitive).count() as u64,
        ..Report::default()
    };
    count_minted(&rows, &mut report);
    let mut movable: HashMap<String, Memory> =
        rows.into_iter().map(|m| (m.id.clone(), m)).collect();
    let mut stored = compare_store(store, &memories, &mut movable, &mut report)?;
    report.store_rows = stored.len() as u64;
    report.store_digest = digest(&stored);
    for m in movable.values() {
        report.would_append += 1;
        report.would_append_bytes += m.content.len() as u64;
        stored.push(as_record(m).canonical_hash());
    }
    report.target_rows = stored.len() as u64;
    report.target_digest = digest(&stored);
    Ok(report)
}

/// Count the rows that would take a generated capture id, overall and by
/// grouping key.
fn count_minted(rows: &[Memory], report: &mut Report) {
    let mut ids: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    for m in rows {
        let Some(id) = minted_capture_id(m) else {
            continue;
        };
        let key = mint_group(m).0.as_str();
        report.minted_rows += 1;
        report
            .minted_by_key
            .entry(key.to_string())
            .or_default()
            .rows += 1;
        ids.entry(key).or_default().insert(id);
    }
    for (key, set) in &ids {
        if let Some(count) = report.minted_by_key.get_mut(*key) {
            count.groups = set.len() as u64;
        }
    }
    report.minted_capture_ids = ids.values().map(|s| s.len() as u64).sum();
}

/// Scan the store, if there is one, against `movable`, removing every id it
/// holds. Returns the canonical hash of each store row.
fn compare_store(
    store: &Store<'_>,
    memories: &Memories<'_>,
    movable: &mut HashMap<String, Memory>,
    report: &mut Report,
) -> Result<Vec<String>> {
    let mut hashes = Vec::new();
    let Some(file) = store_file(store) else {
        return Ok(hashes);
    };
    let dialogs = SqliteDialogs::open_read_only(&file)?;
    let blobs = BlobStore::new(file.with_file_name("blobs"));
    dialogs.scan(&mut |r| {
        let hash = r.canonical_hash();
        match blob_state(&blobs, &r, Depth::Quick)? {
            Blob::Missing => report.blob_missing.push(r.id.clone()),
            Blob::Mismatch => report.blob_mismatch.push(r.id.clone()),
            Blob::Intact => {}
        }
        // A capture may commit after the `memories` read: look it up again
        // before calling the row an orphan.
        let live = match movable.remove(&r.id) {
            Some(m) => Some(m),
            None => memories
                .get_live(&r.id)?
                .filter(|m| m.category == DIALOG_CATEGORY),
        };
        match live {
            None => report.no_memory.push(r.id),
            Some(m) if as_record(&m).canonical_hash() == hash => report.already_present += 1,
            Some(_) => report.drift.push(r.id),
        }
        hashes.push(hash);
        Ok(())
    })?;
    Ok(hashes)
}

/// sha256 over `hashes` sorted, one per line.
fn digest(hashes: &[String]) -> String {
    let mut sorted: Vec<&str> = hashes.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    content_key(sorted.join("\n").as_bytes())
}
