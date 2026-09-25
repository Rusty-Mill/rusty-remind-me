//! Notice when the code a memory refers to changes underneath it (#260).
//!
//! # The filed issue's mechanism does not work
//!
//! #260 proposed reusing [`crate::watcher`]'s scan: anchor a memory to a path
//! inside a configured watch directory, and let the watcher's existing
//! per-file signature check notice when it changes. That cannot fire for the
//! issue's own motivating example. `watcher::collect` only enumerates files
//! whose extension is in [`crate::import_paths::SUPPORTED_SUFFIXES`] —
//! `json`, `md`, `txt`, `pdf`, images, audio. `.rs` is not there, is not
//! going to be there, and should not be: that list means *importable document
//! formats*, not *files worth noticing*. A memory saying "don't touch
//! `auth.rs`" anchored to a watch directory would simply never be rescanned,
//! because the watcher never looks at `.rs` files in the first place.
//!
//! # What this does instead
//!
//! Detecting staleness does not need directory enumeration at all — it needs
//! to re-check a **specific, already-known path**, which is a single `stat`.
//! So there is no watcher integration and no background loop here:
//! [`stale_candidates`] stats each anchored path *on demand*, when asked.
//! That sidesteps the extension filter entirely (any file can be anchored,
//! source code included) and avoids inventing a second background poll next
//! to `watcher`'s.
//!
//! It also means a path never needs to sit inside a *document* watch
//! directory to be anchored. It needs to sit inside a
//! [`CODE_ROOTS_ENV`]-configured root instead — a boundary that exists for
//! the same reason [`crate::import_paths`]'s containment check does: so
//! anchoring is bounded to directories the operator named, not "anything on
//! the machine a memory happens to mention the name of".
//!
//! # Flag, never supersede
//!
//! A changed file does not prove a memory's claim false — the statement may
//! still hold after a refactor. [`stale_candidates`] is read-only and never
//! writes to the memory it reports on, matching the shape of
//! [`crate::contradictions::candidates`] and
//! [`crate::promotion::promotion_candidates`]: a list for a caller to judge,
//! not an automatic demotion.

use crate::import_paths::{expand_home, is_contained, resolve_lexically, split_path_list};
use crate::models::{STALE_CANDIDATES_LIMIT_MAX, STALE_CANDIDATES_LIMIT_MIN};
use rusqlite::{Connection, Result as SqlResult};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

/// Directories a memory's content may be anchored into, in the platform's
/// own `PATH`-list syntax (see [`crate::import_paths::split_path_list`]).
///
/// Unset or empty means **off**: [`detect_code_refs`] returns immediately,
/// before touching the filesystem, matching the `#55`/`#56` convention.
/// Deliberately its own variable rather than reusing
/// [`crate::import_paths::IMPORT_ROOTS_ENV`] or
/// [`crate::watcher::WATCH_DIRS_ENV`] — both name boundaries for *importing
/// documents into the store*, a different operation on a different set of
/// directories than "which of my repos should staleness-tracking watch".
/// Conflating them would mean opening a Documents folder to import also
/// silently turned on code-reference tracking for every file in it.
pub const CODE_ROOTS_ENV: &str = "REMIND_ME_CODE_ROOTS";

/// Longest snippet carried in a stale-candidate listing, matching
/// [`crate::promotion::SNIPPET_CHARS`]'s value and purpose.
const SNIPPET_CHARS: usize = 200;

/// Configured code roots, resolved and `~`-expanded.
///
/// No default-to-home-directory fallback, unlike
/// [`crate::import_paths::import_roots`] — that default matches the
/// reference for an *importer*, where a missing root still needs somewhere
/// safe to read from. This feature has no reference and no such need: unset
/// must mean off, not "silently anchor to anything under `$HOME`".
pub fn configured_code_roots() -> Vec<PathBuf> {
    std::env::var(CODE_ROOTS_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|raw| {
            split_path_list(&raw)
                .into_iter()
                .map(|r| resolve_lexically(&PathBuf::from(expand_home(&r))))
                .collect()
        })
        .unwrap_or_default()
}

/// One file a memory's content names, and its signature when the memory was
/// written.
///
/// `(mtime, size)` rather than a content hash, matching
/// [`crate::watcher`]'s `Signature` — cheap enough to check on every read of
/// [`stale_candidates`] without reading the file's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeRef {
    pub path: String,
    pub mtime: u64,
    pub size: u64,
}

fn signature_of(metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((mtime, metadata.len()))
}

/// Punctuation stripped from both ends of a whitespace-delimited token before
/// it is tried as a path.
///
/// Deliberately narrow: only wrapper characters that commonly *frame* a path
/// reference in prose (`` `auth.rs` ``, "(auth.rs)", "\"auth.rs\"") and would
/// never themselves be part of one. `.` and `,` are left alone on purpose —
/// stripping a trailing `.` would as often destroy a real extension
/// (`config.` from "check `config.py`.") as remove sentence punctuation, and
/// the existence check below is the real filter: `auth.rs.` simply fails to
/// resolve and is dropped, which is a safer failure than guessing.
const WRAPPERS: [char; 6] = ['`', '\'', '"', '(', ')', '<'];

fn path_shaped_tokens(content: &str) -> impl Iterator<Item = &str> {
    content.split_whitespace().filter_map(|raw| {
        let trimmed = raw.trim_matches(|c| WRAPPERS.contains(&c) || c == '>');
        // Require a '.' or '/' before ever calling resolve/stat, so ordinary
        // prose words are not each turned into a filesystem lookup — a
        // hygiene filter, not a correctness one: the existence check below
        // would reject them anyway.
        (trimmed.contains('.') || trimmed.contains('/')).then_some(trimmed)
    })
}

/// Find path-shaped tokens in `content` that resolve to a real file inside a
/// configured code root, and capture each one's current signature.
///
/// Relative tokens (`auth.rs`, `src/auth.rs`) resolve against this process's
/// current directory, via the same [`resolve_lexically`] every other path in
/// this crate goes through — the server's working directory is the only
/// available notion of "which checkout" a bare relative mention refers to.
///
/// Returns an empty vector without any filesystem access when
/// [`configured_code_roots`] is empty, which is what makes this free unless
/// configured.
pub fn detect_code_refs(content: &str) -> Vec<CodeRef> {
    let roots = configured_code_roots();
    if roots.is_empty() {
        return Vec::new();
    }

    let mut seen = HashSet::new();
    let mut refs = Vec::new();

    for token in path_shaped_tokens(content) {
        let resolved = resolve_lexically(&PathBuf::from(token));
        if !is_contained(&resolved, &roots) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&resolved) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let path = resolved.display().to_string();
        if !seen.insert(path.clone()) {
            continue;
        }
        if let Some((mtime, size)) = signature_of(&metadata) {
            refs.push(CodeRef { path, mtime, size });
        }
    }

    refs
}

/// Merge freshly detected refs into a memory's `metadata` object under
/// `code_refs`.
///
/// A no-op on an empty `refs` — call sites do not need to branch on whether
/// anything was found. If `metadata` is not a JSON object (the default is
/// `Null`, and a caller-supplied value could in principle be anything else),
/// it is replaced with a fresh object rather than silently discarding
/// whatever was there; `serde_json::Value::Null` becoming `{}` is the common
/// case this exists for.
pub fn merge_code_refs(metadata: &mut serde_json::Value, refs: &[CodeRef]) {
    if refs.is_empty() {
        return;
    }
    if !metadata.is_object() {
        *metadata = serde_json::json!({});
    }
    metadata["code_refs"] = serde_json::to_value(refs).unwrap_or(serde_json::Value::Null);
}

/// Why an anchored path is being reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    /// Still exists, but its `(mtime, size)` no longer matches.
    Modified,
    /// No longer exists at all.
    Deleted,
}

/// One anchored path that no longer matches what a memory recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleRef {
    pub path: String,
    pub reason: StaleReason,
}

/// A memory with at least one anchor that no longer checks out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleCandidate {
    pub memory_id: String,
    pub content_snippet: String,
    pub stale_refs: Vec<StaleRef>,
}

/// A page of stale-anchor candidates, plus the true backlog behind it.
///
/// Matches `RecalibrateCandidatesResult`'s shape (#283): `total_candidates`
/// is the full, uncapped count of everything [`stale_candidates`] found
/// stale, not just what fit under `limit` — so a caller can tell whether the
/// returned page is the whole backlog or just the front of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleCandidatesResult {
    pub candidates: Vec<StaleCandidate>,
    pub total_candidates: usize,
}

/// Re-check every anchored path and report memories where at least one no
/// longer matches.
///
/// Read-only — see the module doc. `limit` is clamped to
/// [`STALE_CANDIDATES_LIMIT_MIN`]..=[`STALE_CANDIDATES_LIMIT_MAX`], matching
/// [`crate::promotion::promotion_candidates`]'s convention, but unlike that
/// function the bound is applied only in Rust, after staleness is
/// determined — never as a SQL `LIMIT`. The query's `WHERE` clause is a
/// broad pre-filter ("has code_refs at all"); whether a row is actually
/// stale is only known once its anchors are `stat`-ed below. A SQL `LIMIT`
/// would cut the row set before that, and could silently drop a real stale
/// candidate sitting past the first N rows in default order — the opposite
/// of the "accurate first page" this function promises.
///
/// The returned [`StaleCandidatesResult::total_candidates`] is the full
/// count of everything found stale, not just what fit under `limit` (#283):
/// every anchored row is `stat`-checked regardless of `limit`, and only the
/// final list is truncated, so the count behind the page is never silently
/// capped the way [`crate::promotion::backlog`]'s `BACKLOG_PROBE` caps its
/// number.
///
/// # Two checks this reads through that write-time anchoring already applies
///
/// `metadata` is free-form JSON, settable directly by any caller of
/// `remind_me_add`/`remind_me_update` (or carried in unfiltered over sync
/// from a peer), which bypasses [`detect_code_refs`]'s write-time checks
/// entirely. Writing `metadata: {"code_refs": [{"path": "/etc/shadow", ...}]}`
/// by hand and then calling this would otherwise `stat` whatever path was
/// asked for — an unauthenticated existence/mtime oracle over the whole
/// filesystem, exactly what [`crate::import_paths`]'s module doc warns a
/// containment check exists to prevent. So this re-applies both things
/// [`detect_code_refs`] already enforces at write time, rather than trusting
/// that every row reached this table by going through it:
///
/// - **Containment.** A recorded path outside [`configured_code_roots`] is
///   skipped before ever calling `std::fs::metadata` on it — not reported as
///   stale, not reported as current, simply not trusted. Checked before any
///   filesystem access, matching `import_paths`'s own "containment before
///   existence" ordering.
/// - **Sensitivity.** `sensitive = 0` in the query, matching every other
///   ambient read surface ([`crate::digest`], the persona bootstrap) — no
///   `include_sensitive` override, because this is assembled to be read
///   rather than asked for, so there is no per-call intent to opt back in
///   against.
pub fn stale_candidates(conn: &Connection, limit: usize) -> SqlResult<StaleCandidatesResult> {
    let limit = limit.clamp(STALE_CANDIDATES_LIMIT_MIN, STALE_CANDIDATES_LIMIT_MAX);
    let mut stmt = conn.prepare(
        "SELECT id, content, metadata
           FROM memories
          WHERE deleted_at IS NULL
            AND superseded_by IS NULL
            AND sensitive = 0
            AND json_extract(metadata, '$.code_refs') IS NOT NULL",
    )?;

    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<SqlResult<_>>()?;

    let roots = configured_code_roots();
    // No early break on `limit` here -- every row is checked so
    // `total_candidates` below is the real count, not just what fit on the
    // page. Only the returned `candidates` are truncated to `limit`.
    let mut out = Vec::new();
    for (id, content, metadata_json) in rows {
        let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&metadata_json) else {
            continue;
        };
        let Some(recorded) = metadata
            .get("code_refs")
            .and_then(|v| serde_json::from_value::<Vec<CodeRef>>(v.clone()).ok())
        else {
            continue;
        };

        let mut stale_refs = Vec::new();
        for code_ref in recorded {
            let path = PathBuf::from(&code_ref.path);
            if !is_contained(&path, &roots) {
                continue;
            }
            let reason = match std::fs::metadata(&path) {
                Ok(meta) if meta.is_file() => match signature_of(&meta) {
                    Some((mtime, size)) if mtime == code_ref.mtime && size == code_ref.size => None,
                    _ => Some(StaleReason::Modified),
                },
                _ => Some(StaleReason::Deleted),
            };
            if let Some(reason) = reason {
                stale_refs.push(StaleRef {
                    path: code_ref.path,
                    reason,
                });
            }
        }

        if !stale_refs.is_empty() {
            let content_snippet: String = content.chars().take(SNIPPET_CHARS).collect();
            out.push(StaleCandidate {
                memory_id: id,
                content_snippet,
                stale_refs,
            });
        }
    }

    let total_candidates = out.len();
    out.truncate(limit);
    Ok(StaleCandidatesResult {
        candidates: out,
        total_candidates,
    })
}
