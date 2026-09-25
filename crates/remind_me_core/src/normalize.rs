//! Two-phase distillation of raw imports into structured summaries.
//!
//! Imported chat and document content is verbatim and noisy.
//! [`unnormalized_batch`] surfaces raw imports for the calling agent to distil
//! into a `{question, summary, resolution?, refs?}` shape; [`apply_normalizations`]
//! writes each distillation back. The language work happens client-side, the
//! same division [`crate::db::queries::unclassified_batch`] uses for
//! classification — no in-server model dependency.
//!
//! The write is **non-destructive**: a distillation becomes a *new* memory, and
//! the raw row it came from is untouched and stays searchable in its own right.
//! The link is metadata-only, via `normalized_from`.
//!
//! Nothing in this crate writes `document_import` or `chat_import` memories
//! yet — the importers are still to come — so on a store built only through
//! `remind_me_add` the batch is always empty. That is correct rather than
//! broken.

use crate::entity::apply_entity_mentions;
use crate::models::{
    NormalizationEntry, NormalizationError, NormalizationOutcome, NormalizeApplyInput,
    NormalizeApplyResult, NormalizeBatchInput, NormalizeBatchResult, UnnormalizedMemory,
    NORMALIZE_BATCH_MAX, NORMALIZE_BATCH_MIN,
};
use crate::vitality::{calculate_vitality, get_decay_rate, get_source_prior, get_type_prior};
use chrono::Utc;
use rusqlite::{params, Connection, Result};

/// `category` assigned to memories created by [`apply_normalizations`].
pub const NORMALIZED_CATEGORY: &str = "normalized";
/// `source` assigned to them, distinguishing a distillation from the import it
/// came from.
pub const NORMALIZED_SOURCE: &str = "normalization";
/// Sources whose memories are eligible for normalization.
pub const IMPORT_SOURCES: [&str; 2] = ["document_import", "chat_import"];
/// Characters of raw content returned for review.
const SNIPPET_CHARS: usize = 1000;

/// Which raw memories still need normalizing.
///
/// A row qualifies when it is live, came from an importer, and **nothing points
/// back at it**. That last clause is the "already done" test: there is no
/// normalized flag on the row, so a raw import drops out of the backlog once any
/// distillation names it in `normalized_from`.
fn unnormalized_where() -> String {
    let sources = IMPORT_SOURCES
        .iter()
        .map(|s| format!("'{}'", s))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "m.superseded_by IS NULL
         AND m.deleted_at IS NULL
         AND m.source IN ({sources})
         AND NOT EXISTS (
             SELECT 1 FROM memories n
              WHERE json_extract(n.metadata, '$.normalized_from') = m.id
         )",
        sources = sources
    )
}

/// A page of raw imports awaiting normalization, newest first.
///
/// `batch_size` is clamped to 1..=100 rather than rejected, matching
/// `unclassified_batch`.
pub fn unnormalized_batch(
    conn: &Connection,
    input: &NormalizeBatchInput,
) -> Result<NormalizeBatchResult> {
    let batch_size = input
        .batch_size
        .clamp(NORMALIZE_BATCH_MIN, NORMALIZE_BATCH_MAX);
    let predicate = unnormalized_where();

    let total: i64 = conn.query_row(
        &format!("SELECT count(*) FROM memories m WHERE {}", predicate),
        [],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(&format!(
        "SELECT m.id, m.content, m.category, m.source, m.tags, m.metadata
           FROM memories m
          WHERE {}
          ORDER BY m.created_at DESC
          LIMIT ?",
        predicate
    ))?;
    let memories: Vec<UnnormalizedMemory> = stmt
        .query_map(params![batch_size as i64], |row| {
            let content: String = row.get("content")?;
            let tags_json: String = row.get("tags")?;
            let metadata_json: String = row.get("metadata")?;
            let metadata: serde_json::Value =
                serde_json::from_str(&metadata_json).unwrap_or_else(|_| serde_json::json!({}));
            Ok(UnnormalizedMemory {
                id: row.get("id")?,
                // Truncated by characters, not bytes, so a multi-byte character
                // straddling the boundary cannot panic the slice.
                content_snippet: content.chars().take(SNIPPET_CHARS).collect(),
                category: row.get("category")?,
                source: row.get("source")?,
                tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                filename: metadata
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            })
        })?
        .collect::<Result<_>>()?;

    Ok(NormalizeBatchResult {
        memories,
        total_unnormalized: total.max(0) as usize,
    })
}

/// Render a distillation as memory content.
fn normalized_content(entry: &NormalizationEntry) -> String {
    let mut content = format!("**Q:** {}\n\n{}", entry.question, entry.summary);
    if let Some(resolution) = entry.resolution.as_deref().filter(|r| !r.is_empty()) {
        content.push_str(&format!("\n\n**Resolution:** {}", resolution));
    }
    content
}

/// Write distillations back as new memories linked to their raw imports.
///
/// Each entry creates a new memory rather than modifying the raw one, so the
/// verbatim import stays searchable. The new memory inherits the raw row's
/// `tags`, `doc_id` and `chunk_index` — the last two so neighbour-aware
/// retrieval still associates it with the rest of the document — and links any
/// entities the entry names.
///
/// An unknown `memory_id` is reported in `errors` rather than failing the
/// batch, so one bad reference does not discard the other 49.
pub fn apply_normalizations(
    conn: &Connection,
    input: &NormalizeApplyInput,
) -> Result<NormalizeApplyResult> {
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let mut results = Vec::new();
    let mut errors = Vec::new();

    for entry in &input.normalizations {
        let raw: Option<(String, Option<String>, Option<i64>)> = conn
            .query_row(
                "SELECT tags, doc_id, chunk_index FROM memories WHERE id = ?",
                params![entry.memory_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();

        let (tags_json, doc_id, chunk_index) = match raw {
            Some(row) => row,
            None => {
                errors.push(NormalizationError {
                    memory_id: entry.memory_id.clone(),
                    error: "memory not found".to_string(),
                });
                continue;
            }
        };

        let content = normalized_content(entry);
        let normalized_id = format!("mem_{}", uuid::Uuid::new_v4().simple());

        let mut metadata = serde_json::json!({
            "normalized_from": entry.memory_id,
            "question": entry.question,
            "refs": entry.refs,
        });
        if let Some(resolution) = entry.resolution.as_deref().filter(|r| !r.is_empty()) {
            metadata["resolution"] = serde_json::json!(resolution);
        }

        // A distillation is a fresh memory, so it gets the same write-time
        // vitality treatment as one written through `remind_me_add`; otherwise
        // it would sit at the column defaults and rank unlike everything else.
        let decay_rate = get_decay_rate(NORMALIZED_CATEGORY);
        let base_weight = get_type_prior(NORMALIZED_CATEGORY) * get_source_prior(NORMALIZED_SOURCE);
        let vitality = calculate_vitality(base_weight, 0, decay_rate, &now_iso, now);

        let (node_id, client) = crate::sync::memory_provenance();
        conn.execute(
            "INSERT INTO memories (
                id, content, category, tags, source, metadata, created_at, updated_at,
                doc_id, chunk_index, decay_rate, vitality, base_weight, access_count, accessed_at,
                node_id, client
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?)",
            params![
                normalized_id,
                content,
                NORMALIZED_CATEGORY,
                tags_json,
                NORMALIZED_SOURCE,
                metadata.to_string(),
                now_iso,
                now_iso,
                doc_id,
                chunk_index,
                decay_rate,
                vitality,
                base_weight,
                now_iso,
                node_id,
                client,
            ],
        )?;

        apply_entity_mentions(conn, &normalized_id, &entry.entities)?;

        results.push(NormalizationOutcome {
            memory_id: entry.memory_id.clone(),
            normalized_id,
        });
    }

    Ok(NormalizeApplyResult {
        normalized: results.len(),
        results,
        errors,
    })
}
