//! Rebuild check for the dialog store (#528, D133). Read only.
//!
//! Replays every live `memories` dialog row (the record, D82) from what the
//! store keeps into a scratch dialog store, then compares. Deterministic
//! stages only; model-written extraction comes in a later slice.
//!
//! - redaction: a stored body scrubs to itself;
//! - replay: a row with an archived source span re-renders, through the
//!   capture's own render and scrub, to its stored body, so its scratch row
//!   (live id and capture id kept) has the live canonical hash (D77). A row
//!   with no span is counted, not a finding;
//! - store: a replayed row the dialog store holds has the same hash there;
//!   one it lacks is counted;
//! - skeletons: every node range still addresses the capture's dialog,
//!   rebuilt from the replayed rows when all of them replayed, else the
//!   live rows; the report counts each kind;
//! - links: every derived memory's `capture_id` names a live dialog. A
//!   memory linked only through `source_capture_id` (a fact decomposed from
//!   a capture) is not validated here; the report counts those links.
//!
//! Replay reads `REMIND_ME_REDACT` and `REMIND_ME_MESSAGE_TOOLS` as they are
//! now; no row records their value at capture (Q36), so the report names
//! both.

use crate::archive::SpanRead;
use crate::blobs::BlobStore;
use crate::db::dialogs::{DialogLog, DialogStore, NewDialog};
use crate::db::memories::Memories;
use crate::db::sqlite::SqliteDialogs;
use crate::db::{Result, Store, StoreError};
use crate::dialog_reconcile::{as_record, store_file};
use crate::models::{Memory, CONTINUATION_TYPE, DIALOG_CATEGORY, SKELETON_CATEGORY, UNCLASSIFIED};
use crate::transcript::{MessageTools, MESSAGE_TOOLS_ENV};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// What [`check`] found. Lists hold memory ids, except `bad_skeleton`
/// (capture ids).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The scratch directory the rows were rebuilt into.
    pub scratch: PathBuf,
    /// Whether `scratch` was left on disk.
    pub kept: bool,
    /// `REMIND_ME_REDACT` is not `0`.
    pub redact: bool,
    /// `REMIND_ME_MESSAGE_TOOLS` as set (`None`: unset, the defaults).
    pub message_tools: Option<String>,
    /// Live `memories` dialog rows.
    pub dialogs: u64,
    pub replayed: u64,
    /// Rows with no archived source span.
    pub not_replayable: u64,
    /// Rows whose archived source holds a redaction and whose replay
    /// differs from the live row. The capture rendered the raw text and then
    /// redacted it, but the archive was redacted first, so a cut (the dialog
    /// cap, a tool preview) or a re-serialised value can land elsewhere.
    /// Listed for a look, not counted as findings or as replayed.
    pub redacted_source_differs: Vec<String>,
    /// Rows whose recorded span is not read (inverted, outside its blob, or
    /// over `archive::MAX_SPAN_BYTES`), each with the reason. Listed for a
    /// look, not counted as findings.
    pub unsupported_span: Vec<String>,
    /// Replayed rows the dialog store does not hold.
    pub not_in_store: u64,
    /// Bodies that a scrub would still change.
    pub unstable_redaction: Vec<String>,
    /// Replayed rows whose canonical hash differs from the live row's.
    pub replay_mismatch: Vec<String>,
    /// Replayed rows whose canonical hash differs from the dialog store's.
    pub store_drift: Vec<String>,
    pub bad_skeleton: Vec<String>,
    /// Skeletons checked against the replayed dialog.
    pub skeletons_replayed: u64,
    /// Skeletons checked against the live rows only, because some row of
    /// their capture was not replayed: unverified by replay.
    pub skeletons_live_only: u64,
    /// Derived memories whose `capture_id` names no live dialog.
    pub orphaned: Vec<String>,
    /// Memories that name a capture through `source_capture_id`. Counted,
    /// not validated: this check covers `capture_id` links only.
    pub source_capture_links_unchecked: u64,
    /// Memories by kind (`memory_type`, unset as `unclassified`) on both
    /// sides: every live memory, and the rows the scratch rebuilt. Report
    /// only, never a finding. This slice rebuilds dialog rows only, so every
    /// other kind shows `rebuilt` 0 until the extraction slice (D133).
    pub by_kind: BTreeMap<String, KindCount>,
}

/// One kind's count on each side of the rebuild.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindCount {
    pub live: u64,
    pub rebuilt: u64,
}

/// The kind a memory is counted under in [`Report::by_kind`].
fn kind_of(m: &Memory) -> &str {
    m.memory_type.as_deref().unwrap_or(UNCLASSIFIED)
}

impl Report {
    /// Problems found; the counts are not.
    pub fn findings(&self) -> usize {
        self.unstable_redaction.len()
            + self.replay_mismatch.len()
            + self.store_drift.len()
            + self.bad_skeleton.len()
            + self.orphaned.len()
    }
}

/// A fresh `rebuild-check-<time>` directory beside `store`'s database;
/// `None` for an in-memory database.
pub fn default_scratch(store: &Store<'_>) -> Option<PathBuf> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    Some(
        store
            .path()?
            .parent()?
            .join(format!("rebuild-check-{stamp}")),
    )
}

/// Run the check with `scratch`, or [`default_scratch`]. The scratch is
/// removed afterwards unless `keep` is set or something was found; an
/// error leaves it for inspection.
///
/// # Errors
///
/// [`StoreError`] if `scratch` exists or cannot be made owner-only (on a
/// platform other than Unix it is refused: see [`private_dir`]), or a store
/// cannot be read.
pub fn check(store: &Store<'_>, scratch: Option<&Path>, keep: bool) -> Result<Report> {
    let scratch = match scratch {
        Some(dir) => dir.to_path_buf(),
        None => default_scratch(store).ok_or_else(|| {
            StoreError::Engine("an in-memory database needs an explicit scratch".into())
        })?,
    };
    private_dir(&scratch)
        .map_err(|e| StoreError::Engine(format!("scratch {}: {e}", scratch.display())))?;
    let mut report = rebuild(store, &scratch)?;
    report.kept = keep || report.findings() > 0;
    if !report.kept {
        if let Err(e) = std::fs::remove_dir_all(&scratch) {
            eprintln!(
                "rusty-remind-me: scratch {} not removed: {e}",
                scratch.display()
            );
            report.kept = true;
        }
    }
    Ok(report)
}

/// Make `dir`, which must not exist (its rows would mix with the replay),
/// readable by this user only. It holds replayed conversation bodies and
/// can be kept, so it is created 0700 on Unix, never readable by others even
/// briefly, and checked after. Everything inside is reached through it.
/// Elsewhere it is refused (fail closed): setting and checking a Windows ACL
/// is not implemented.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().mode(0o700).create(dir)?;
        let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(std::io::Error::other(format!(
                "created with mode {mode:o}, not 700"
            )));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Err(std::io::Error::other(
            "an owner-only scratch is supported on Unix only; nothing was created",
        ))
    }
}

fn rebuild(store: &Store<'_>, scratch: &Path) -> Result<Report> {
    let message_tools = std::env::var(MESSAGE_TOOLS_ENV).ok();
    let mut report = Report {
        scratch: scratch.to_path_buf(),
        redact: crate::redact::enabled_by_env(),
        message_tools,
        ..Report::default()
    };
    let (dialogs, derived): (Vec<Memory>, Vec<Memory>) = Memories::new(store)
        .all_live()?
        .into_iter()
        .partition(|m| m.category == DIALOG_CATEGORY);
    for m in dialogs.iter().chain(&derived) {
        report
            .by_kind
            .entry(kind_of(m).to_string())
            .or_default()
            .live += 1;
    }
    let mut log = DialogLog::new(
        SqliteDialogs::open(scratch)?,
        BlobStore::new(scratch.join("blobs")),
    );
    let tools = MessageTools::from_env();
    // Replayed bodies are kept only for captures with a skeleton, to check
    // its ranges against them.
    let sketched = skeleton_captures(&derived);
    let mut kept: HashMap<String, String> = HashMap::new();
    for m in &dialogs {
        report.dialogs += 1;
        if !redaction_stable(&m.content) {
            report.unstable_redaction.push(m.id.clone());
        }
        match replay(store, m, &tools)? {
            Replay::Body(body) => {
                report.replayed += 1;
                log.record(&replayed_row(m, &body))?;
                if m.capture_id
                    .as_deref()
                    .is_some_and(|c| sketched.contains(c))
                {
                    kept.insert(m.id.clone(), body);
                }
            }
            Replay::NoSource => report.not_replayable += 1,
            Replay::RedactedSourceDiffers => report.redacted_source_differs.push(m.id.clone()),
            Replay::Unsupported(why) => report.unsupported_span.push(format!("{}: {why}", m.id)),
        }
    }
    compare(&mut report, log.store(), &dialogs, &stored_hashes(store)?)?;
    check_skeletons(&mut report, store, &sketched, &kept)?;
    report.orphaned = orphans(&dialogs, &derived);
    report.source_capture_links_unchecked = derived
        .iter()
        .filter(|m| m.source_capture_id.is_some())
        .count() as u64;
    Ok(report)
}

/// Whether scrubbing `body` again leaves it as it is.
fn redaction_stable(body: &str) -> bool {
    let (mut tags, mut metadata) = (Vec::new(), serde_json::json!({}));
    crate::boundary::scrub(body, &mut tags, &mut metadata) == body
}

/// What replaying one row gave.
enum Replay {
    Body(String),
    /// No archived span, or no capture id.
    NoSource,
    /// See [`Report::redacted_source_differs`].
    RedactedSourceDiffers,
    /// See [`Report::unsupported_span`].
    Unsupported(String),
}

/// `m`'s body rebuilt from its archived span.
fn replay(store: &Store<'_>, m: &Memory, tools: &MessageTools) -> Result<Replay> {
    if m.capture_id.is_none() {
        return Ok(Replay::NoSource);
    }
    let bytes = match crate::archive::span_bytes(store, &m.id)
        .map_err(|e| StoreError::Engine(format!("source of {}: {e}", m.id)))?
    {
        SpanRead::Bytes(bytes) => bytes,
        SpanRead::Missing => return Ok(Replay::NoSource),
        SpanRead::Unsupported(why) => return Ok(Replay::Unsupported(why)),
    };
    let continuation =
        m.metadata.get("type").and_then(serde_json::Value::as_str) == Some(CONTINUATION_TYPE);
    let span = String::from_utf8_lossy(&bytes);
    let body = crate::transcript::replay_row(&span, continuation, tools);
    if span.contains(crate::redact::REDACTED_PREFIX) && !same_row(m, &body) {
        return Ok(Replay::RedactedSourceDiffers);
    }
    Ok(Replay::Body(body))
}

/// Whether `m` with `body` in place of its own hashes as `m` does.
fn same_row(m: &Memory, body: &str) -> bool {
    let replayed = Memory {
        content: body.to_string(),
        ..m.clone()
    };
    as_record(&replayed).canonical_hash() == as_record(m).canonical_hash()
}

/// The scratch row for `m`: its own fields, the replayed body.
fn replayed_row<'a>(m: &'a Memory, body: &'a str) -> NewDialog<'a> {
    NewDialog {
        id: &m.id,
        capture_id: m.capture_id.as_deref().unwrap_or_default(),
        session_id: m.session_id.as_deref(),
        project: m.project.as_deref(),
        created_at: &m.created_at,
        body,
    }
}

/// Canonical hash of each row in the dialog store beside `store`, if any.
fn stored_hashes(store: &Store<'_>) -> Result<HashMap<String, String>> {
    let mut hashes = HashMap::new();
    let Some(file) = store_file(store) else {
        return Ok(hashes);
    };
    SqliteDialogs::open_read_only(&file)?.scan(&mut |r| {
        hashes.insert(r.id.clone(), r.canonical_hash());
        Ok(())
    })?;
    Ok(hashes)
}

/// Compare each scratch row with its live row and its stored row.
fn compare(
    report: &mut Report,
    scratch: &SqliteDialogs,
    dialogs: &[Memory],
    stored: &HashMap<String, String>,
) -> Result<()> {
    let live: HashMap<&str, (String, &str)> = dialogs
        .iter()
        .map(|m| (m.id.as_str(), (as_record(m).canonical_hash(), kind_of(m))))
        .collect();
    scratch.scan(&mut |r| {
        let hash = r.canonical_hash();
        let row = live.get(r.id.as_str());
        if let Some((_, kind)) = row {
            report
                .by_kind
                .entry((*kind).to_string())
                .or_default()
                .rebuilt += 1;
        }
        if row.map(|(h, _)| h) != Some(&hash) {
            report.replay_mismatch.push(r.id.clone());
        }
        match stored.get(&r.id) {
            None => report.not_in_store += 1,
            Some(h) if *h != hash => report.store_drift.push(r.id.clone()),
            Some(_) => {}
        }
        Ok(())
    })
}

/// Captures that have a skeleton.
fn skeleton_captures(derived: &[Memory]) -> BTreeSet<String> {
    derived
        .iter()
        .filter(|m| m.category == SKELETON_CATEGORY)
        .filter_map(|m| m.capture_id.clone())
        .collect()
}

/// Check each skeleton's ranges, against the replayed dialog where `kept`
/// holds every row of it.
fn check_skeletons(
    report: &mut Report,
    store: &Store<'_>,
    captures: &BTreeSet<String>,
    kept: &HashMap<String, String>,
) -> Result<()> {
    for capture in captures {
        let checked =
            crate::skeleton::check_rebuilt(store, capture, kept).map_err(|e| match e {
                crate::skeleton::SkeletonError::Db(e) => e,
                other => StoreError::Engine(format!("skeleton of {capture}: {other}")),
            })?;
        let Some(checked) = checked else {
            continue;
        };
        match checked.replayed {
            true => report.skeletons_replayed += 1,
            false => report.skeletons_live_only += 1,
        }
        if !checked.holds {
            report.bad_skeleton.push(capture.clone());
        }
    }
    Ok(())
}

/// Derived memories whose `capture_id` names no live dialog.
fn orphans(dialogs: &[Memory], derived: &[Memory]) -> Vec<String> {
    let captures: HashSet<&str> = dialogs
        .iter()
        .filter_map(|m| m.capture_id.as_deref())
        .collect();
    derived
        .iter()
        .filter(|m| {
            m.capture_id
                .as_deref()
                .is_some_and(|c| !captures.contains(c))
        })
        .map(|m| m.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(not(unix))]
    fn a_scratch_is_refused_where_it_cannot_be_owner_only() {
        let dir = std::env::temp_dir().join(format!("rebuild_{}", uuid::Uuid::new_v4().simple()));
        assert!(super::private_dir(&dir).is_err());
        assert!(!dir.exists(), "nothing is created");
    }

    #[test]
    #[cfg(unix)]
    fn a_scratch_is_created_owner_only_and_never_reused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rebuild_{}", uuid::Uuid::new_v4().simple()));
        super::private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert!(
            super::private_dir(&dir).is_err(),
            "an existing directory is refused"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
