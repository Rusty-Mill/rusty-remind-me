//! Per-memory edit history and rollback.
//!
//! # Where revisions are written, and where they deliberately are not
//!
//! Issue #109 warns that "a revision row has to be written by every existing
//! mutation path" and lists seven tools, then asks for that list to be audited
//! against the reference rather than assumed. The audit answer is that the
//! reference writes revisions from **exactly one** place: its update path
//! (`tools/crud.py`'s `_apply_memory_field_update`). Reclassify, normalize,
//! annotate, consolidate and decompose record nothing.
//!
//! That is followed here rather than "corrected", and the reasoning holds up:
//! a revision exists to recover a value a human replaced, and the other paths
//! either add derived data alongside the original (normalize, decompose,
//! annotate) or change classification metadata that is itself recomputable
//! (reclassify). Recording all of them would bury the edits worth reverting in
//! machine-generated noise.
//!
//! # What counts as an edit
//!
//! Only the columns an update can change — content, category, tags, metadata,
//! sensitive — and only when the incoming value genuinely differs from what is
//! stored. Two consequences fall out, both wanted:
//!
//! - a same-value update creates no revision, mirroring the outbox trigger's
//!   "only on genuine change" discipline (issue #100);
//! - access tracking, which writes `accessed_at`/`access_count` and no tracked
//!   column, never produces one. A vault would otherwise accumulate a revision
//!   per read.

use crate::models::{MemoryRevision, RevertOutcome};
use rusqlite::{params, Connection, OptionalExtension, Result};

/// Snapshot a memory's current tracked columns before an update overwrites
/// them.
///
/// Call **before** applying the update, inside the same transaction, so a
/// crash between the two cannot leave one without the other.
///
/// `reason` is free text stored on the captured revision — a plain update
/// leaves it `None`; a revert records what it was reverting to.
///
/// Returns whether a revision was actually written, which is false when
/// nothing tracked changed.
pub fn capture_revision(
    conn: &Connection,
    memory_id: &str,
    incoming: &TrackedChanges,
    reason: Option<&str>,
) -> Result<bool> {
    if incoming.is_empty() {
        return Ok(false);
    }

    let current: Option<(String, String, String, String, Option<i64>)> = conn
        .query_row(
            "SELECT content, category, tags, metadata, sensitive
               FROM memories WHERE id = ?",
            params![memory_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4).ok())),
        )
        .optional()?;

    let Some((content, category, tags, metadata, sensitive)) = current else {
        return Ok(false);
    };

    if !incoming.differs_from(&content, &category, &tags, &metadata, sensitive) {
        return Ok(false);
    }

    conn.execute(
        "INSERT INTO memory_revisions
             (memory_id, content, category, tags, metadata, sensitive,
              edited_at, revision_reason)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            memory_id,
            content,
            category,
            tags,
            metadata,
            sensitive,
            chrono::Utc::now().to_rfc3339(),
            reason,
        ],
    )?;
    Ok(true)
}

/// The tracked columns an update is about to set, in their stored form.
///
/// Deliberately the *stored* representation — tags and metadata as their JSON
/// strings — so the comparison against the current row is like-for-like. A
/// comparison done on the parsed values would call a metadata re-serialisation
/// with reordered keys a change and record a spurious revision.
#[derive(Debug, Default, Clone)]
pub struct TrackedChanges {
    pub content: Option<String>,
    pub category: Option<String>,
    pub tags_json: Option<String>,
    pub metadata_json: Option<String>,
    pub sensitive: Option<bool>,
}

impl TrackedChanges {
    fn is_empty(&self) -> bool {
        self.content.is_none()
            && self.category.is_none()
            && self.tags_json.is_none()
            && self.metadata_json.is_none()
            && self.sensitive.is_none()
    }

    fn differs_from(
        &self,
        content: &str,
        category: &str,
        tags: &str,
        metadata: &str,
        sensitive: Option<i64>,
    ) -> bool {
        self.content.as_deref().is_some_and(|v| v != content)
            || self.category.as_deref().is_some_and(|v| v != category)
            || self.tags_json.as_deref().is_some_and(|v| v != tags)
            || self.metadata_json.as_deref().is_some_and(|v| v != metadata)
            || self
                .sensitive
                .is_some_and(|v| v != (sensitive.unwrap_or(0) != 0))
    }
}

/// A memory's revisions, newest first.
///
/// Ordered by `edited_at` then `id` so revisions captured within the same
/// clock tick still come back in the order they were written — otherwise a
/// burst of edits would list in an arbitrary order and the ids a caller passes
/// to revert would not mean what the list implied.
pub fn history(conn: &Connection, memory_id: &str, limit: usize) -> Result<Vec<MemoryRevision>> {
    let mut stmt = conn.prepare(
        "SELECT id, memory_id, content, category, tags, metadata, sensitive,
                edited_at, revision_reason
           FROM memory_revisions
          WHERE memory_id = ?
          ORDER BY edited_at DESC, id DESC
          LIMIT ?",
    )?;
    let rows = stmt
        .query_map(params![memory_id, limit as i64], |r| {
            Ok(MemoryRevision {
                id: r.get(0)?,
                memory_id: r.get(1)?,
                content: r.get(2)?,
                category: r.get(3)?,
                tags: r.get(4)?,
                metadata: r.get(5)?,
                sensitive: r.get::<_, Option<i64>>(6)?.map(|v| v != 0),
                edited_at: r.get(7)?,
                revision_reason: r.get(8)?,
            })
        })?
        .collect();
    rows
}

/// Whether a memory exists and is not soft-deleted.
pub fn memory_is_live(conn: &Connection, memory_id: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM memories WHERE id = ? AND deleted_at IS NULL",
            params![memory_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Restore a memory's tracked columns to a prior revision.
///
/// Reverting is itself an edit: it bumps `updated_at`, enters the sync outbox
/// like any other change, and captures a revision of the state *just before*
/// the revert — so a revert can itself be reverted.
///
/// The revision id must belong to this memory. One that does not is an error
/// rather than a silent no-op, because the two are indistinguishable to a
/// caller who mistyped an id.
pub fn revert(
    conn: &Connection,
    memory_id: &str,
    revision_id: i64,
    reason: Option<&str>,
) -> Result<RevertOutcome> {
    if !memory_is_live(conn, memory_id)? {
        return Ok(RevertOutcome::MemoryNotFound);
    }

    let revision: Option<(String, String, String, String, Option<i64>)> = conn
        .query_row(
            "SELECT content, category, tags, metadata, sensitive
               FROM memory_revisions WHERE id = ? AND memory_id = ?",
            params![revision_id, memory_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4).ok())),
        )
        .optional()?;

    let Some((content, category, tags, metadata, sensitive)) = revision else {
        return Ok(RevertOutcome::RevisionNotFound);
    };

    // A revision captured before the `sensitive` column existed has no value
    // for it. Falling back to "not sensitive" rather than refusing keeps old
    // revisions revertable, which is the whole point of keeping them.
    let sensitive = sensitive.unwrap_or(0) != 0;

    let changes = TrackedChanges {
        content: Some(content.clone()),
        category: Some(category.clone()),
        tags_json: Some(tags.clone()),
        metadata_json: Some(metadata.clone()),
        sensitive: Some(sensitive),
    };
    let stated = reason
        .map(str::to_string)
        .unwrap_or_else(|| format!("revert to revision {}", revision_id));
    let captured = capture_revision(conn, memory_id, &changes, Some(&stated))?;

    if !captured {
        // Nothing tracked differs, so the memory already holds this revision's
        // values. Reporting that beats writing a no-op revision and an outbox
        // row that says nothing changed.
        return Ok(RevertOutcome::NoChange);
    }

    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE memories
            SET content = ?, category = ?, tags = ?, metadata = ?,
                sensitive = ?, updated_at = ?
          WHERE id = ?",
        params![
            content,
            category,
            tags,
            metadata,
            sensitive as i64,
            now,
            memory_id
        ],
    )?;

    // Content changed means the stored vectors describe text that is no longer
    // there. Best-effort, like every other embed in this crate: a missing
    // embedder leaves the memory keyword-searchable rather than failing an
    // edit that already committed.
    if let Some(embedder) = crate::embedder::available_embedder() {
        let _ = crate::vectors::embed_and_store(conn, &*embedder, memory_id, &content);
    }

    Ok(RevertOutcome::Reverted { revision_id })
}

/// How much of a revision's content the markdown preview shows.
///
/// Matches the reference's `_REVISION_PREVIEW_CHARS` (`formatting.py:91`).
const REVISION_PREVIEW_CHARS: usize = 200;

/// Render one revision as the reference's `_fmt_revision_md` does.
fn render_revision_markdown(revision: &MemoryRevision) -> String {
    // Truncate by *characters*, not bytes: slicing a multi-byte codepoint
    // would panic, and a revision is arbitrary user content.
    let mut preview: String = revision
        .content
        .chars()
        .take(REVISION_PREVIEW_CHARS)
        .collect();
    if revision.content.chars().count() > REVISION_PREVIEW_CHARS {
        preview.push('…');
    }
    let mut line = format!(
        "- **Revision `{}`** ({}) — category: {} — {}",
        revision.id, revision.edited_at, revision.category, preview
    );
    if let Some(reason) = revision
        .revision_reason
        .as_ref()
        .filter(|r| !r.trim().is_empty())
    {
        line.push_str(&format!("  _{}_", reason));
    }
    line
}

/// Render a memory's revision history the way the reference's `_fmt_revisions`
/// does (`formatting.py:114`), including the empty-history sentence.
///
/// Kept byte-compatible with the reference for the same reason
/// `reminders::render_memories_markdown` is: this text is what a model reads
/// back, and a different shape is a different prompt.
pub fn render_revisions_markdown(memory_id: &str, revisions: &[MemoryRevision]) -> String {
    if revisions.is_empty() {
        return format!("_No revision history for memory `{}`._", memory_id);
    }
    let mut lines = vec![format!(
        "**{} revision(s) for memory `{}`, newest first:**\n",
        revisions.len(),
        memory_id
    )];
    lines.extend(revisions.iter().map(render_revision_markdown));
    lines.join("\n")
}
