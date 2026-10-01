//! The curation queues on the engine core (ADR-0023, core PR 3c, built
//! dark): captures awaiting decomposition, raw imports awaiting
//! normalization, contradiction candidates and the maintenance counts.
//! [`crate::db::curation`] calls these when its store carries the core.

use super::core::CoreTables;
use super::memories::{self, MemoryRow};
use super::{core_ref, EngineTables};
use crate::db::curation::{Backlog, CaptureActivity, CaptureRow, ImportRow, NormalizationSource};
use crate::db::{Result, StoreError};
use crate::models::{ContradictionSide, Memory};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};

fn live(row: &MemoryRow) -> bool {
    row.deleted_at.is_none() && row.superseded_by.is_none()
}

fn newest_first(rows: &mut [MemoryRow]) {
    rows.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));
}

fn tags(row: &MemoryRow) -> Vec<String> {
    serde_json::from_str(&row.tags).unwrap_or_default()
}

// --- captures ---------------------------------------------------------------

/// Every memory carrying `capture_id`, by category, ties by id.
pub(crate) fn capture_rows(tables: &EngineTables, capture_id: &str) -> Result<Vec<Memory>> {
    let mut rows: Vec<MemoryRow> = memories::rows(core_ref(tables)?)
        .filter(|row| row.capture_id.as_deref() == Some(capture_id))
        .collect();
    rows.sort_by(|a, b| (&a.category, &a.id).cmp(&(&b.category, &b.id)));
    Ok(rows.iter().map(MemoryRow::to_memory).collect())
}

/// The tags of the lowest-id memory carrying `capture_id`.
pub(crate) fn capture_tags(tables: &EngineTables, capture_id: &str) -> Result<Option<Vec<String>>> {
    Ok(memories::rows(core_ref(tables)?)
        .filter(|row| row.capture_id.as_deref() == Some(capture_id))
        .min_by(|a, b| a.id.cmp(&b.id))
        .map(|row| tags(&row)))
}

/// Captures, not themselves decomposed facts and not deleted, that no
/// memory names as its `source_capture_id`, newest first.
fn undecomposed_rows(core: &CoreTables) -> Vec<MemoryRow> {
    let named: HashSet<String> = memories::rows(core)
        .filter_map(|row| row.source_capture_id)
        .collect();
    let mut rows: Vec<MemoryRow> = memories::rows(core)
        .filter(|row| row.deleted_at.is_none() && row.source_capture_id.is_none())
        .filter(|row| row.capture_id.as_ref().is_some_and(|c| !named.contains(c)))
        .collect();
    newest_first(&mut rows);
    rows
}

pub(crate) fn undecomposed(tables: &EngineTables, limit: usize) -> Result<Vec<CaptureRow>> {
    Ok(undecomposed_rows(core_ref(tables)?)
        .into_iter()
        .take(limit)
        .map(|row| CaptureRow {
            tags: tags(&row),
            capture_id: row.capture_id.unwrap_or_default(),
            id: row.id,
            content: row.content,
            category: row.category,
        })
        .collect())
}

pub(crate) fn count_undecomposed(tables: &EngineTables) -> Result<i64> {
    Ok(count(undecomposed_rows(core_ref(tables)?).len()))
}

pub(crate) fn capture_activity(tables: &EngineTables) -> Result<CaptureActivity> {
    let mut captures = HashSet::new();
    let mut last: Option<String> = None;
    for row in memories::rows(core_ref(tables)?).filter(|row| row.deleted_at.is_none()) {
        let Some(capture) = row.capture_id else {
            continue;
        };
        captures.insert(capture);
        if last.as_deref().is_none_or(|l| row.created_at.as_str() > l) {
            last = Some(row.created_at);
        }
    }
    Ok(CaptureActivity {
        captures: count(captures.len()),
        last_capture_at: last,
    })
}

// --- normalization ----------------------------------------------------------

/// The ids memories name as `metadata.normalized_from` (text only, as
/// `json_extract(…) = m.id` compares).
fn normalized_ids(core: &CoreTables) -> HashSet<String> {
    memories::rows(core)
        .filter_map(|row| {
            let metadata: Value = serde_json::from_str(&row.metadata).ok()?;
            metadata
                .get("normalized_from")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn unnormalized_rows(core: &CoreTables, sources: &[&str]) -> Vec<MemoryRow> {
    let normalized = normalized_ids(core);
    let mut rows: Vec<MemoryRow> = memories::rows(core)
        .filter(|row| live(row) && sources.contains(&row.source.as_str()))
        .filter(|row| !normalized.contains(&row.id))
        .collect();
    newest_first(&mut rows);
    rows
}

pub(crate) fn unnormalized(
    tables: &EngineTables,
    sources: &[&str],
    limit: usize,
) -> Result<Vec<ImportRow>> {
    Ok(unnormalized_rows(core_ref(tables)?, sources)
        .into_iter()
        .take(limit)
        .map(|row| ImportRow {
            tags: tags(&row),
            metadata: serde_json::from_str(&row.metadata).unwrap_or_else(|_| serde_json::json!({})),
            id: row.id,
            content: row.content,
            category: row.category,
            source: row.source,
        })
        .collect())
}

pub(crate) fn count_unnormalized(tables: &EngineTables, sources: &[&str]) -> Result<i64> {
    Ok(count(unnormalized_rows(core_ref(tables)?, sources).len()))
}

pub(crate) fn normalization_source(
    tables: &EngineTables,
    memory_id: &str,
) -> Result<Option<NormalizationSource>> {
    Ok(
        memories::row(core_ref(tables)?, memory_id).map(|row| NormalizationSource {
            tags: tags(&row),
            doc_id: row.doc_id,
            chunk_index: row.chunk_index,
        }),
    )
}

// --- maintenance ------------------------------------------------------------

pub(crate) fn backlog_depth(tables: &EngineTables, backlog: Backlog) -> Result<i64> {
    let core = core_ref(tables)?;
    let depth = match backlog {
        Backlog::Undecomposed => undecomposed_rows(core).len(),
        Backlog::Unannotated => {
            let linked: HashSet<String> = super::graph::link_pairs(core)
                .into_iter()
                .map(|(memory_id, _)| memory_id)
                .collect();
            memories::rows(core)
                .filter(|row| live(row) && row.category != "dialog")
                .filter(|row| row.subject.is_none() && row.predicate.is_none())
                .filter(|row| row.object.is_none() && !linked.contains(&row.id))
                .count()
        }
        Backlog::Unnormalized => unnormalized_rows(core, &["document_import", "chat_import"]).len(),
        Backlog::Unclassified => memories::rows(core)
            .filter(|row| row.deleted_at.is_none() && row.memory_type == "unclassified")
            .count(),
    };
    Ok(count(depth))
}

// --- contradictions ---------------------------------------------------------

/// `lower(trim(text))` as SQLite computes it: spaces trimmed, ASCII folded.
fn folded(text: &str) -> String {
    text.trim_matches(' ').to_ascii_lowercase()
}

/// Whether both rows have a subject and predicate and they match folded.
fn same_claim(a: &MemoryRow, b: &MemoryRow) -> bool {
    match (&a.subject, &a.predicate, &b.subject, &b.predicate) {
        (Some(sa), Some(pa), Some(sb), Some(pb)) => {
            folded(sa) == folded(sb) && folded(pa) == folded(pb)
        }
        _ => false,
    }
}

/// Every candidate pair `(id_a, id_b)` with `id_a < id_b`, in order.
fn pairs(core: &CoreTables, max_fanout: i64) -> BTreeSet<(String, String)> {
    let mut by_entity: HashMap<String, Vec<String>> = HashMap::new();
    for (memory_id, entity_id) in super::graph::link_pairs(core) {
        by_entity.entry(entity_id).or_default().push(memory_id);
    }
    let eligible =
        |id: &str| memories::row(core, id).filter(|row| live(row) && row.category != "dialog");
    let mut found = BTreeSet::new();
    for ids in by_entity.values() {
        if i64::try_from(ids.len()).unwrap_or(i64::MAX) > max_fanout {
            continue;
        }
        for a in ids {
            for b in ids.iter().filter(|b| *b > a) {
                let (Some(ra), Some(rb)) = (eligible(a), eligible(b)) else {
                    continue;
                };
                if !same_claim(&ra, &rb) {
                    found.insert((a.clone(), b.clone()));
                }
            }
        }
    }
    found
}

pub(crate) fn count_contradiction_pairs(tables: &EngineTables, max_fanout: i64) -> Result<i64> {
    Ok(count(pairs(core_ref(tables)?, max_fanout).len()))
}

pub(crate) fn contradiction_pairs(
    tables: &EngineTables,
    max_fanout: i64,
    cursor: Option<(&str, &str)>,
    limit: usize,
) -> Result<Vec<(String, String)>> {
    Ok(pairs(core_ref(tables)?, max_fanout)
        .into_iter()
        .filter(|(a, b)| cursor.is_none_or(|(ca, cb)| (a.as_str(), b.as_str()) > (ca, cb)))
        .take(limit)
        .collect())
}

/// The names of the stored entities both memories mention, by name.
pub(crate) fn shared_entity_names(
    tables: &EngineTables,
    id_a: &str,
    id_b: &str,
) -> Result<Vec<String>> {
    let core = core_ref(tables)?;
    let links = super::graph::link_pairs(core);
    let of = |id: &str| -> HashSet<String> {
        links
            .iter()
            .filter(|(m, _)| m == id)
            .map(|(_, e)| e.clone())
            .collect()
    };
    let (a, b) = (of(id_a), of(id_b));
    let names: BTreeSet<String> = a
        .intersection(&b)
        .filter_map(|e| super::graph::entity_name(core, e))
        .collect();
    Ok(names.into_iter().collect())
}

/// One side of a pair, its content cut to `snippet_chars` characters. A
/// missing memory is [`StoreError::NotFound`], as the SQL's `query_row` is.
pub(crate) fn contradiction_side(
    tables: &EngineTables,
    memory_id: &str,
    snippet_chars: usize,
) -> Result<ContradictionSide> {
    let row = memories::row(core_ref(tables)?, memory_id).ok_or(StoreError::NotFound)?;
    Ok(ContradictionSide {
        content_snippet: row.content.chars().take(snippet_chars).collect(),
        memory_type: Some(row.memory_type),
        id: row.id,
        category: row.category,
        subject: row.subject,
        predicate: row.predicate,
        object: row.object,
        created_at: row.created_at,
    })
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}
