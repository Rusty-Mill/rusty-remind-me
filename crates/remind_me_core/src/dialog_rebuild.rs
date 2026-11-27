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
//! - skeletons: every node range still addresses the capture's dialog;
//! - links: every derived memory's `capture_id` names a live dialog.
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
use crate::models::{Memory, CONTINUATION_TYPE, DIALOG_CATEGORY, SKELETON_CATEGORY};
use crate::transcript::{MessageTools, MESSAGE_TOOLS_ENV};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
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
    /// First rows cut to `MAX_DIALOG_CHARS` whose source holds a
    /// redaction: the cut cannot be placed again, so they are not replayed.
    pub cut_after_redaction: u64,
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
    /// Derived memories whose `capture_id` names no live dialog.
    pub orphaned: Vec<String>,
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
    let mut log = DialogLog::new(
        SqliteDialogs::open(scratch)?,
        BlobStore::new(scratch.join("blobs")),
    );
    let tools = MessageTools::from_env();
    for m in &dialogs {
        report.dialogs += 1;
        if !redaction_stable(&m.content) {
            report.unstable_redaction.push(m.id.clone());
        }
        match replay(store, m, &tools)? {
            Replay::Body(body) => {
                report.replayed += 1;
                log.record(&replayed_row(m, &body))?;
            }
            Replay::NoSource => report.not_replayable += 1,
            Replay::CutAfterRedaction => report.cut_after_redaction += 1,
            Replay::Unsupported(why) => report.unsupported_span.push(format!("{}: {why}", m.id)),
        }
    }
    compare(&mut report, log.store(), &dialogs, &stored_hashes(store)?)?;
    report.bad_skeleton = bad_skeletons(store, &derived)?;
    report.orphaned = orphans(&dialogs, &derived);
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
    /// See [`Report::cut_after_redaction`].
    CutAfterRedaction,
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
    Ok(crate::transcript::replay_row(&span, continuation, tools)
        .map_or(Replay::CutAfterRedaction, Replay::Body))
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
    let live: HashMap<&str, String> = dialogs
        .iter()
        .map(|m| (m.id.as_str(), as_record(m).canonical_hash()))
        .collect();
    scratch.scan(&mut |r| {
        let hash = r.canonical_hash();
        if live.get(r.id.as_str()) != Some(&hash) {
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

/// Captures whose skeleton has a node outside its dialog.
fn bad_skeletons(store: &Store<'_>, derived: &[Memory]) -> Result<Vec<String>> {
    let captures: BTreeSet<&str> = derived
        .iter()
        .filter(|m| m.category == SKELETON_CATEGORY)
        .filter_map(|m| m.capture_id.as_deref())
        .collect();
    let mut bad = Vec::new();
    for capture in captures {
        let holds = crate::skeleton::ranges_hold(store, capture).map_err(|e| match e {
            crate::skeleton::SkeletonError::Db(e) => e,
            other => StoreError::Engine(format!("skeleton of {capture}: {other}")),
        })?;
        if !holds {
            bad.push(capture.to_string());
        }
    }
    Ok(bad)
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
