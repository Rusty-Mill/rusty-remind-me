//! Conversation capture: a verbatim dialog and its summary, stored as two
//! linked memories.
//!
//! The pair shares a `capture_id`, which is the handle everything downstream
//! keys on — `remind_me_get_capture` to retrieve both halves, and
//! `remind_me_decompose` to break the capture into atomic facts.

use crate::db::queries::{parse_memory_row, MEMORY_COLUMNS};
use crate::entity::{
    apply_entity_mentions, maybe_link_entity_relation, supersede_contradicting_facts,
};
use crate::models::{
    AutoCaptureInput, Capture, CaptureResult, DecomposeBatchInput, DecomposeBatchResult,
    DecomposeInput, DecomposeResult, Memory, UndecomposedCapture, CAPTURE_SOURCE,
    CAPTURE_TITLE_CHARS, DECOMPOSE_BATCH_MAX, DECOMPOSE_BATCH_MIN, DECOMPOSITION_SOURCE,
    DIALOG_CATEGORY, FACT_CATEGORY, UNCLASSIFIED,
};
use crate::vitality::{calculate_vitality, get_decay_rate, get_source_prior, get_type_prior};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Result};

/// Derive a display title from a summary when the caller supplied none.
///
/// First line, capped — a summary's opening line is the closest thing to a
/// headline it has.
fn derive_title(title: &str, summary: &str) -> String {
    let supplied = title.trim();
    if !supplied.is_empty() {
        return supplied.to_string();
    }
    summary
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(CAPTURE_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Merge the caller's metadata with the fields that make a capture a capture.
fn capture_metadata(
    base: &serde_json::Value,
    capture_id: &str,
    title: &str,
    kind: &str,
    link_key: &str,
    link_value: &str,
) -> serde_json::Value {
    let mut metadata = match base {
        serde_json::Value::Object(map) => serde_json::Value::Object(map.clone()),
        _ => serde_json::json!({}),
    };
    let object = metadata.as_object_mut().expect("just built an object");
    object.insert("capture_id".into(), serde_json::json!(capture_id));
    object.insert("title".into(), serde_json::json!(title));
    object.insert("type".into(), serde_json::json!(kind));
    object.insert(link_key.into(), serde_json::json!(link_value));
    metadata
}

#[allow(clippy::too_many_arguments)]
fn insert_half(
    conn: &Connection,
    id: &str,
    content: &str,
    category: &str,
    tags_json: &str,
    metadata: &serde_json::Value,
    capture_id: &str,
    now_iso: &str,
) -> Result<()> {
    let now = Utc::now();
    let decay_rate = get_decay_rate(category);
    let base_weight = get_type_prior(category) * get_source_prior(CAPTURE_SOURCE);
    let vitality = calculate_vitality(base_weight, 0, decay_rate, now_iso, now);

    let (node_id, client) = crate::sync::memory_provenance();
    conn.execute(
        "INSERT INTO memories (
            id, content, category, tags, source, metadata, capture_id,
            created_at, updated_at, decay_rate, vitality, base_weight,
            access_count, accessed_at, node_id, client
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?)",
        params![
            id,
            content,
            category,
            tags_json,
            CAPTURE_SOURCE,
            metadata.to_string(),
            capture_id,
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
    Ok(())
}

/// Store a conversation as a linked dialog/summary pair.
///
/// The **dialog's category is always [`DIALOG_CATEGORY`]**; the caller's
/// `category` names the summary. That asymmetry is load-bearing rather than
/// incidental: `extract_batch` excludes `dialog`, because a raw transcript is
/// not a candidate for triple extraction — its facts come out through
/// `decompose` instead. Storing the transcript under the caller's category
/// would flood the annotation backlog with raw conversations.
///
/// Both halves are linked three ways: the `capture_id` column, a `capture_id`
/// in each one's metadata, and a direct pointer at the other half
/// (`linked_dialog` / `linked_summary`). The dialog's pointer is written in a
/// second pass, because the summary's id does not exist when the dialog is
/// inserted.
pub fn auto_capture(conn: &Connection, input: &AutoCaptureInput) -> Result<CaptureResult> {
    let now_iso = Utc::now().to_rfc3339();
    let capture_id = format!("cap_{}", uuid::Uuid::new_v4().simple());
    let dialog_id = format!("mem_{}", uuid::Uuid::new_v4().simple());
    let summary_id = format!("mem_{}", uuid::Uuid::new_v4().simple());
    let title = derive_title(&input.title, &input.summary);
    let tags_json = serde_json::to_string(&input.tags).unwrap_or_else(|_| "[]".to_string());

    let dialog_meta = capture_metadata(
        &input.metadata,
        &capture_id,
        &title,
        "dialog",
        "linked_summary",
        &summary_id,
    );
    let summary_meta = capture_metadata(
        &input.metadata,
        &capture_id,
        &title,
        "summary",
        "linked_dialog",
        &dialog_id,
    );

    insert_half(
        conn,
        &dialog_id,
        &input.conversation,
        DIALOG_CATEGORY,
        &tags_json,
        &dialog_meta,
        &capture_id,
        &now_iso,
    )?;
    insert_half(
        conn,
        &summary_id,
        &input.summary,
        &input.category,
        &tags_json,
        &summary_meta,
        &capture_id,
        &now_iso,
    )?;

    Ok(CaptureResult {
        capture_id,
        dialog_id,
        summary_id,
        title,
        tags: input.tags.clone(),
        category: input.category.clone(),
    })
}

/// Retrieve both halves of a capture.
///
/// Looks up the indexed `capture_id` column rather than scanning metadata.
/// Returns `None` when nothing carries the id.
///
/// The halves are told apart by their metadata `type`. Anything sharing the id
/// but matching neither is returned in `other` rather than dropped — a capture
/// that has lost a half, or gained a third row through sync, should be visible
/// rather than silently half-reported.
pub fn get_capture(conn: &Connection, capture_id: &str) -> Result<Option<Capture>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM memories WHERE capture_id = ? ORDER BY category",
        MEMORY_COLUMNS
    ))?;
    let rows: Vec<Memory> = stmt
        .query_map(params![capture_id], parse_memory_row)?
        .collect::<Result<_>>()?;
    if rows.is_empty() {
        return Ok(None);
    }

    let kind_of = |memory: &Memory| -> Option<String> {
        memory
            .metadata
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };

    let mut dialog = None;
    let mut summary = None;
    let mut other = Vec::new();
    for memory in rows {
        match kind_of(&memory).as_deref() {
            Some("dialog") if dialog.is_none() => dialog = Some(memory),
            Some("summary") if summary.is_none() => summary = Some(memory),
            _ => other.push(memory),
        }
    }

    let title = summary
        .as_ref()
        .or(dialog.as_ref())
        .and_then(|m| m.metadata.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled")
        .to_string();

    Ok(Some(Capture {
        capture_id: capture_id.to_string(),
        title,
        dialog,
        summary,
        other,
    }))
}

/// Break a capture into individually searchable atomic facts.
///
/// Each fact becomes its own memory, linked to the capture through
/// `source_capture_id`. Facts are *not* captures themselves, so their own
/// `capture_id` stays NULL — that is what keeps a decomposed fact out of
/// [`undecomposed_batch`] and stops decomposition generating its own backlog.
///
/// Three things happen per fact beyond the insert, in this order, and the order
/// matters:
///
/// 1. **Entity mentions are applied**, which upserts any entities the fact
///    names.
/// 2. **A relation edge is recorded** when the triple's subject and object both
///    resolve to *known* entities — usually the ones step 1 just created. Doing
///    this before step 1 would find nothing.
/// 3. **Contradicted facts are superseded.**
///
/// Returns `None` when no memory carries `capture_id`.
pub fn decompose(conn: &Connection, input: &DecomposeInput) -> Result<Option<DecomposeResult>> {
    let parent_tags: Option<Vec<String>> = conn
        .query_row(
            "SELECT tags FROM memories WHERE capture_id = ? LIMIT 1",
            params![input.capture_id],
            |row| {
                let tags_json: String = row.get(0)?;
                Ok(serde_json::from_str(&tags_json).unwrap_or_default())
            },
        )
        .optional()?;
    let Some(parent_tags) = parent_tags else {
        return Ok(None);
    };

    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let mut fact_ids = Vec::new();
    let mut entities_linked = 0;
    let mut relations_linked = 0;
    let mut superseded_ids = Vec::new();

    for fact in &input.facts {
        let fact_id = format!("mem_{}", uuid::Uuid::new_v4().simple());
        let memory_type = fact
            .memory_type
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or(UNCLASSIFIED);

        // Tags: the parent's, then the fact's own, de-duplicated and
        // order-preserving — the same merge idiom `upsert_entity` uses for
        // aliases.
        let mut merged_tags = parent_tags.clone();
        for tag in &fact.extra_tags {
            if !merged_tags.contains(tag) {
                merged_tags.push(tag.clone());
            }
        }

        // `memory_type` owns `decay_rate` — the single-writer arrangement
        // established when reclassify landed — and seeds `base_weight`, so a
        // decision outranks an unclassified aside before any feedback or access
        // signal exists. At zero elapsed days vitality equals base_weight
        // exactly, so both columns carry the same seeded value.
        let decay_rate = get_decay_rate(memory_type);
        let base_weight = get_type_prior(memory_type);

        let (node_id, client) = crate::sync::memory_provenance();

        // #260: a no-op unless REMIND_ME_CODE_ROOTS is configured. Merged
        // into the metadata object already built for `source_capture_id`
        // rather than a separate write, so one INSERT still carries both.
        let mut metadata = serde_json::json!({ "source_capture_id": input.capture_id });
        let code_refs = crate::code_refs::detect_code_refs(&fact.content);
        crate::code_refs::merge_code_refs(&mut metadata, &code_refs);

        conn.execute(
            "INSERT INTO memories (
                id, content, category, tags, source, metadata,
                capture_id, source_capture_id, created_at, updated_at,
                memory_type, decay_rate, vitality, base_weight, status,
                accessed_at, access_count, subject, predicate, object,
                node_id, client
             ) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, 'active', ?, 0, ?, ?, ?, ?, ?)",
            params![
                fact_id,
                fact.content,
                FACT_CATEGORY,
                serde_json::to_string(&merged_tags).unwrap_or_else(|_| "[]".to_string()),
                DECOMPOSITION_SOURCE,
                metadata.to_string(),
                input.capture_id,
                now_iso,
                now_iso,
                memory_type,
                decay_rate,
                base_weight,
                base_weight,
                now_iso,
                fact.subject,
                fact.predicate,
                fact.object,
                node_id,
                client,
            ],
        )?;

        entities_linked += apply_entity_mentions(conn, &fact_id, &fact.entities)?;
        if maybe_link_entity_relation(
            conn,
            fact.subject.as_deref(),
            fact.predicate.as_deref(),
            fact.object.as_deref(),
        )? {
            relations_linked += 1;
        }
        superseded_ids.extend(supersede_contradicting_facts(
            conn,
            &fact_id,
            fact.subject.as_deref(),
            fact.predicate.as_deref(),
            fact.object.as_deref(),
        )?);

        fact_ids.push(fact_id);
    }

    Ok(Some(DecomposeResult {
        created: fact_ids.len(),
        fact_ids,
        capture_id: input.capture_id.clone(),
        parent_tags_inherited: parent_tags,
        entities_linked,
        relations_linked,
        superseded_ids,
    }))
}

/// Captures that have not been decomposed yet, newest first.
///
/// A row qualifies when it **is** a capture (`capture_id` set), is **not itself
/// a fact** (`source_capture_id` unset), and nothing already names its
/// `capture_id` as a source. That last clause is the "already done" test —
/// there is no decomposed flag, so a capture leaves the backlog once any fact
/// points back at it.
pub fn undecomposed_batch(
    conn: &Connection,
    input: &DecomposeBatchInput,
) -> Result<DecomposeBatchResult> {
    let batch_size = input
        .batch_size
        .clamp(DECOMPOSE_BATCH_MIN, DECOMPOSE_BATCH_MAX);
    let predicate = "m.capture_id IS NOT NULL
         AND m.source_capture_id IS NULL
         AND m.deleted_at IS NULL
         AND NOT EXISTS (
             SELECT 1 FROM memories c WHERE c.source_capture_id = m.capture_id
         )";

    let total: i64 = conn.query_row(
        &format!("SELECT count(*) FROM memories m WHERE {}", predicate),
        [],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(&format!(
        "SELECT m.id, m.capture_id, m.content, m.category, m.tags
           FROM memories m
          WHERE {}
          ORDER BY m.created_at DESC
          LIMIT ?",
        predicate
    ))?;
    let memories: Vec<UndecomposedCapture> = stmt
        .query_map(params![batch_size as i64], |row| {
            let content: String = row.get("content")?;
            let tags_json: String = row.get("tags")?;
            Ok(UndecomposedCapture {
                id: row.get("id")?,
                capture_id: row.get("capture_id")?,
                content_snippet: content.chars().take(500).collect(),
                category: row.get("category")?,
                tags: serde_json::from_str(&tags_json).unwrap_or_default(),
            })
        })?
        .collect::<Result<_>>()?;

    Ok(DecomposeBatchResult {
        memories,
        total_undecomposed: total.max(0) as usize,
    })
}
