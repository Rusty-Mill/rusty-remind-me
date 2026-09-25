//! Symbolic compression: a capture's shape, cheap to read, with drill-down.
//!
//! # The gap this fills
//!
//! [`crate::capture`] stores a conversation as two linked memories: the
//! verbatim dialog and a summary. There is nothing between them. A caller
//! wanting more than the summary has only the whole transcript, and on a long
//! session that is thousands of tokens to answer a question about its shape.
//!
//! A **skeleton** is a third artifact at that missing altitude: a Mermaid
//! diagram of the conversation's structure, whose nodes each name a line range
//! in the dialog. Reading it costs a diagram; following one node costs one
//! turn. Neither costs the transcript.
//!
//! # Who draws it
//!
//! The calling agent's model, handed back through [`write_skeleton`] — exactly
//! how `remind_me_decompose` already works. Nothing here calls an LLM, so this
//! adds no model dependency to the server.
//!
//! # Line ranges, and why they are validated at write time
//!
//! Nodes address the dialog by inclusive, 1-based line range. A model can
//! count lines; it cannot reliably count characters, and a character offset
//! that is wrong by forty is indistinguishable from a correct one until
//! someone reads the slice it returns.
//!
//! Every range is therefore checked against the dialog's actual line count
//! *before* the skeleton is stored. A skeleton that would return the wrong
//! bytes is refused outright rather than written and discovered later —
//! drill-down whose answers are subtly wrong is worse than no drill-down,
//! because it reads as authoritative.
//!
//! # Stored as a capture row, not a new table
//!
//! The skeleton is a third memory sharing the `capture_id`, distinguished by
//! its metadata `type` exactly as the dialog and summary are. That means
//! [`crate::capture::get_capture`] already finds it (in `other`, until it
//! learns the name), sync already carries it, and deleting a capture already
//! takes it along. A separate table would have needed all three rebuilt.

use crate::capture::get_capture;
use crate::models::{
    Skeleton, SkeletonSlice, SkeletonWriteInput, CAPTURE_SOURCE, SKELETON_CATEGORY,
};
use crate::vitality::{calculate_vitality, get_decay_rate, get_source_prior, get_type_prior};
use chrono::Utc;
use rusqlite::{params, Connection};
use std::collections::BTreeMap;

/// Why a skeleton could not be written or read.
#[derive(Debug)]
pub enum SkeletonError {
    Db(rusqlite::Error),
    /// No memory carries this `capture_id`.
    NoCapture(String),
    /// The capture has no dialog half, so there is nothing for nodes to
    /// address. A summary-only capture cannot be drilled into.
    NoDialog(String),
    /// A node's range falls outside the dialog, or is inverted.
    BadRange {
        node: String,
        start: usize,
        end: usize,
        lines: usize,
    },
    /// The diagram carried no nodes, so nothing could ever be drilled into.
    NoNodes,
}

impl std::fmt::Display for SkeletonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "{}", e),
            Self::NoCapture(id) => write!(f, "no capture found with id {:?}", id),
            Self::NoDialog(id) => write!(
                f,
                "capture {:?} has no dialog half, so its skeleton would have nothing to point at",
                id
            ),
            Self::BadRange {
                node,
                start,
                end,
                lines,
            } => write!(
                f,
                "node {:?} spans lines {}..={}, but the dialog has {} line(s). \
                 Ranges are inclusive and 1-based.",
                node, start, end, lines
            ),
            Self::NoNodes => write!(
                f,
                "a skeleton needs at least one node mapped to a line range, \
                 otherwise the diagram cannot be drilled into"
            ),
        }
    }
}

impl std::error::Error for SkeletonError {}

impl From<rusqlite::Error> for SkeletonError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

/// How a dialog is split into addressable lines.
///
/// One function, used by both the write-time validation and the read-time
/// slice, so a range that validated cannot then resolve against a different
/// division of the same text. Two spellings of "split into lines" is exactly
/// the drift that makes an off-by-one appear only in production.
fn dialog_lines(content: &str) -> Vec<&str> {
    content.lines().collect()
}

/// Store (or replace) a capture's skeleton.
///
/// Replacing rather than appending: a capture has one shape, and a second
/// skeleton would leave [`read_skeleton`] picking arbitrarily between them.
pub fn write_skeleton(
    conn: &Connection,
    input: &SkeletonWriteInput,
) -> Result<Skeleton, SkeletonError> {
    if input.nodes.is_empty() {
        return Err(SkeletonError::NoNodes);
    }

    let capture = get_capture(conn, &input.capture_id)?
        .ok_or_else(|| SkeletonError::NoCapture(input.capture_id.clone()))?;
    let dialog = capture
        .dialog
        .as_ref()
        .ok_or_else(|| SkeletonError::NoDialog(input.capture_id.clone()))?;

    let line_count = dialog_lines(&dialog.content).len();
    for (node, (start, end)) in &input.nodes {
        // `start == 0` is caught by the same check: ranges are 1-based, so a
        // model that emitted 0-based offsets fails loudly on its first node
        // rather than silently returning one line too many forever.
        if *start == 0 || start > end || *end > line_count {
            return Err(SkeletonError::BadRange {
                node: node.clone(),
                start: *start,
                end: *end,
                lines: line_count,
            });
        }
    }

    // Any previous skeleton goes first, so a replace cannot briefly leave two.
    conn.execute(
        "DELETE FROM memories WHERE capture_id = ? AND category = ?",
        params![input.capture_id, SKELETON_CATEGORY],
    )?;

    let now_iso = Utc::now().to_rfc3339();
    let now = Utc::now();
    let skeleton_id = format!("mem_{}", uuid::Uuid::new_v4().simple());
    let title = dialog
        .metadata
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled")
        .to_string();

    let metadata = serde_json::json!({
        "capture_id": input.capture_id,
        "title": title,
        "type": "skeleton",
        "linked_dialog": dialog.id,
        "nodes": input.nodes,
    });

    let decay_rate = get_decay_rate(SKELETON_CATEGORY);
    let base_weight = get_type_prior(SKELETON_CATEGORY) * get_source_prior(CAPTURE_SOURCE);
    let vitality = calculate_vitality(base_weight, 0, decay_rate, &now_iso, now);

    let (node_id, client) = crate::sync::memory_provenance();
    conn.execute(
        "INSERT INTO memories (
            id, content, category, tags, source, metadata, capture_id,
            created_at, updated_at, decay_rate, vitality, base_weight,
            access_count, accessed_at, node_id, client
         ) VALUES (?, ?, ?, '[]', ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?)",
        params![
            skeleton_id,
            input.mermaid,
            SKELETON_CATEGORY,
            CAPTURE_SOURCE,
            metadata.to_string(),
            input.capture_id,
            now_iso,
            now_iso,
            decay_rate,
            vitality,
            base_weight,
            now_iso,
            node_id,
            client,
        ],
    )?;

    Ok(Skeleton {
        capture_id: input.capture_id.clone(),
        skeleton_id,
        mermaid: input.mermaid.clone(),
        nodes: input.nodes.clone(),
    })
}

/// Parse the node map back out of a stored skeleton's metadata.
///
/// A node whose value is not a two-element range is skipped rather than
/// failing the read: the rest of the diagram is still usable, and a skeleton
/// that refuses to load because one node is malformed strands the whole
/// capture at transcript altitude.
fn nodes_from_metadata(metadata: &serde_json::Value) -> BTreeMap<String, (usize, usize)> {
    let Some(map) = metadata.get("nodes").and_then(|v| v.as_object()) else {
        return BTreeMap::new();
    };
    map.iter()
        .filter_map(|(node, value)| {
            let pair = value.as_array()?;
            let start = pair.first()?.as_u64()? as usize;
            let end = pair.get(1)?.as_u64()? as usize;
            Some((node.clone(), (start, end)))
        })
        .collect()
}

/// A capture's skeleton, or `None` when it has none.
pub fn read_skeleton(
    conn: &Connection,
    capture_id: &str,
) -> Result<Option<Skeleton>, SkeletonError> {
    let Some(capture) = get_capture(conn, capture_id)? else {
        return Ok(None);
    };

    // Found through `other` rather than a dedicated field: `get_capture` sorts
    // by metadata `type` and knows two names. Reading it here rather than
    // teaching `Capture` a third field keeps the skeleton additive — an older
    // build, or a `remind_me` sharing the database, still round-trips the row
    // untouched instead of treating it as a malformed capture.
    let Some(row) = capture
        .other
        .iter()
        .find(|m| m.category == SKELETON_CATEGORY)
    else {
        return Ok(None);
    };

    Ok(Some(Skeleton {
        capture_id: capture_id.to_string(),
        skeleton_id: row.id.clone(),
        mermaid: row.content.clone(),
        nodes: nodes_from_metadata(&row.metadata),
    }))
}

/// The dialog lines one skeleton node stands for.
///
/// `None` when the capture has no skeleton, or the skeleton has no such node —
/// the caller asked about something that is not there, which is not an error
/// so much as an empty answer.
pub fn node_slice(
    conn: &Connection,
    capture_id: &str,
    node: &str,
) -> Result<Option<SkeletonSlice>, SkeletonError> {
    let Some(skeleton) = read_skeleton(conn, capture_id)? else {
        return Ok(None);
    };
    let Some(&(start, end)) = skeleton.nodes.get(node) else {
        return Ok(None);
    };

    let capture = get_capture(conn, capture_id)?
        .ok_or_else(|| SkeletonError::NoCapture(capture_id.to_string()))?;
    let dialog = capture
        .dialog
        .ok_or_else(|| SkeletonError::NoDialog(capture_id.to_string()))?;

    let lines = dialog_lines(&dialog.content);
    // Ranges were validated against this dialog at write time, but the dialog
    // is an ordinary memory and `remind_me_update` can shorten it afterwards.
    // Clamping rather than erroring keeps an edited capture readable; the
    // returned line numbers say what was actually served.
    if start == 0 || start > lines.len() {
        return Ok(None);
    }
    let end = end.min(lines.len());

    Ok(Some(SkeletonSlice {
        capture_id: capture_id.to_string(),
        node: node.to_string(),
        start_line: start,
        end_line: end,
        content: lines[start - 1..end].join("\n"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_survive_a_metadata_round_trip() {
        let metadata = serde_json::json!({ "nodes": { "n1": [1, 4], "n2": [5, 9] } });
        let nodes = nodes_from_metadata(&metadata);
        assert_eq!(nodes.get("n1"), Some(&(1, 4)));
        assert_eq!(nodes.get("n2"), Some(&(5, 9)));
    }

    #[test]
    fn one_malformed_node_does_not_lose_the_others() {
        let metadata = serde_json::json!({
            "nodes": { "good": [1, 2], "truncated": [3], "wrong_type": "1-2" }
        });
        let nodes = nodes_from_metadata(&metadata);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes.get("good"), Some(&(1, 2)));
    }

    #[test]
    fn metadata_without_nodes_reads_as_empty_rather_than_panicking() {
        assert!(nodes_from_metadata(&serde_json::json!({})).is_empty());
    }
}
