use crate::expansion::{self, MemorySearchResponse};
use crate::fts::sanitize_fts_query;
use crate::models::{
    AnnotateInput, AnnotateResult, AnnotationApplied, AnnotationError, BulkDeleteResult,
    BulkTagInput, BulkTagResult, ExtractBatchInput, ExtractBatchResult, Memory, MemoryAddInput,
    MemoryListInput, MemoryListResult, MemorySearchInput, MemorySearchResult, MemoryUpdateInput,
    ReclassifyBatchInput, ReclassifyBatchResult, ReclassifyInput, ReclassifyResult,
    SearchPageInput, SearchPageResult, TagMode, UnannotatedMemory, UnclassifiedMemory,
    UpdateOutcome, EXTRACT_BATCH_MAX, EXTRACT_BATCH_MIN, LIST_LIMIT_MAX, LIST_LIMIT_MIN,
    RECLASSIFY_BATCH_MAX, RECLASSIFY_BATCH_MIN, UNCLASSIFIED,
};
use crate::retrieval::{
    choose_rrf_weights, rank_rrf, rrf_k_from_env, trim_by_token_budget, RrfConfig, RrfFusion,
    RrfSignals,
};
use crate::vitality::{
    apply_feedback_adjustment, calculate_vitality, get_decay_rate, get_source_prior,
    get_type_prior, EFFECTIVE_VITALITY_FN, VITALITY_FLOOR,
};
use chrono::Utc;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, Result, Row};

/// Columns selected wherever a full [`Memory`] is parsed via [`parse_memory_row`].
///
/// This list must stay a superset of what [`parse_memory_row`] reads, and the
/// serialised [`Memory`] must in turn cover every column of `memories` — see
/// `memory_json_test.rs`, which asserts exactly that against the live schema.
/// A column added to the schema but not here silently vanished from every
/// tool response before #198.
pub const MEMORY_COLUMNS: &str = "id, content, category, tags, source, metadata, created_at, \
     updated_at, capture_id, subject, predicate, object, superseded_by, decay_rate, vitality, \
     base_weight, access_count, accessed_at, doc_id, chunk_index, remind_at, sensitive, \
     memory_type, status, node_id, client, source_capture_id, deleted_at";

/// [`MEMORY_COLUMNS`] with each name qualified by `alias`, for queries that join.
pub fn prefixed_memory_columns(alias: &str) -> String {
    MEMORY_COLUMNS
        .split(',')
        .map(|c| format!("{}.{}", alias, c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn parse_memory_row(row: &Row) -> Result<Memory> {
    let tags_json: String = row.get("tags")?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();

    let meta_json: String = row.get("metadata")?;
    let metadata: serde_json::Value =
        serde_json::from_str(&meta_json).unwrap_or(serde_json::Value::Null);

    let created_at: String = row.get("created_at")?;

    Ok(Memory {
        id: row.get("id")?,
        content: row.get("content")?,
        category: row.get("category")?,
        tags,
        source: row.get("source")?,
        metadata,
        created_at: created_at.clone(),
        updated_at: row.get("updated_at")?,
        capture_id: row.get("capture_id")?,
        subject: row.get("subject")?,
        predicate: row.get("predicate")?,
        object: row.get("object")?,
        superseded_by: row.get("superseded_by")?,
        decay_rate: row.get("decay_rate")?,
        vitality: row.get("vitality")?,
        base_weight: row.get("base_weight")?,
        access_count: row.get("access_count")?,
        // The column is nullable and a row written by `remind_me` may leave it
        // unset. Falling back to `created_at` matches the reference, and keeps
        // `get::<String>` from failing on NULL.
        accessed_at: row
            .get::<_, Option<String>>("accessed_at")?
            .unwrap_or_else(|| created_at.clone()),
        doc_id: row.get("doc_id")?,
        chunk_index: row.get("chunk_index")?,
        remind_at: row.get("remind_at")?,
        // SQLite has no boolean type and the column is nullable, so this
        // arrives as 0/1/NULL. Same shape trap as `SyncRecord::sensitive`.
        sensitive: row.get::<_, Option<i64>>("sensitive")?.unwrap_or(0) != 0,
        // Read as Option even where the column is NOT NULL: this database is
        // shared with `remind_me`, and a row written before a column gained
        // its default arrives as NULL. A non-optional get would fail the whole
        // read rather than report a missing value.
        memory_type: row.get("memory_type")?,
        status: row.get("status")?,
        node_id: row.get("node_id")?,
        client: row.get("client")?,
        source_capture_id: row.get("source_capture_id")?,
        deleted_at: row.get("deleted_at")?,
    })
}

pub fn add_memory(conn: &Connection, mut input: MemoryAddInput) -> Result<Memory> {
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let id = format!("mem_{}", uuid::Uuid::new_v4().simple());

    // #260: a no-op unless REMIND_ME_CODE_ROOTS is configured.
    let code_refs = crate::code_refs::detect_code_refs(&input.content);
    crate::code_refs::merge_code_refs(&mut input.metadata, &code_refs);

    let tags_json = serde_json::to_string(&input.tags).unwrap_or_else(|_| "[]".to_string());
    let metadata_json = serde_json::to_string(&input.metadata).unwrap_or_else(|_| "{}".to_string());
    let decay_rate = get_decay_rate(&input.category);
    let type_prior = get_type_prior(&input.category);
    let source_prior = get_source_prior(&input.source);
    let base_weight = type_prior * source_prior;
    let initial_vitality = calculate_vitality(base_weight, 0, decay_rate, &now_iso, now);

    conn.execute(
        "INSERT INTO memories (
            id, content, category, tags, source, metadata, created_at, updated_at,
            subject, predicate, object, decay_rate, vitality, base_weight, access_count, accessed_at,
            node_id, client, sensitive
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?)",
        params![
            id,
            input.content,
            input.category,
            tags_json,
            input.source,
            metadata_json,
            now_iso,
            now_iso,
            input.subject,
            input.predicate,
            input.object,
            decay_rate,
            initial_vitality,
            base_weight,
            now_iso,
            crate::sync::configured_node_id(),
            crate::sync::configured_client(),
            input.sensitive,
        ],
    )?;

    // `MemoryAddInput::entities` was previously parsed and then dropped, so a
    // caller supplying entity mentions got a silent no-op. Same path as
    // `annotate_memories` so both behave identically.
    crate::entity::apply_entity_mentions(conn, &id, &input.entities)?;

    // Best-effort: no embedder configured, or one that fails mid-request,
    // leaves this memory keyword-searchable only — never a reason to fail
    // the write that already succeeded. `remind_me_reindex` is the backstop
    // for anything that lands here without an embedder available.
    if let Some(embedder) = crate::embedder::available_embedder() {
        let _ = crate::vectors::embed_and_store(conn, &*embedder, &id, &input.content);
    }

    let memory = get_memory_by_id(conn, &id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;

    // Local mutation, so automation hears about it. A record arriving from a
    // peer goes through `sync::upsert_record` instead, which deliberately
    // emits nothing — see `events`' module docs for why echoing sync writes
    // would make two nodes chase each other forever.
    crate::events::emit(crate::events::Event::Created, &memory.id, &memory.category);

    Ok(memory)
}

pub fn get_memory_by_id(conn: &Connection, id: &str) -> Result<Option<Memory>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM memories WHERE id = ? AND deleted_at IS NULL",
        MEMORY_COLUMNS
    ))?;

    let mut rows = stmt.query_map(params![id], parse_memory_row)?;
    if let Some(row) = rows.next() {
        row.map(Some)
    } else {
        Ok(None)
    }
}

/// Build the shared `WHERE` clause and its bindings for [`list_memories`].
///
/// Tag matching is ALL-of, one `EXISTS` per requested tag, against the
/// normalized `memory_tags` index. That table is kept in step with the JSON
/// `tags` column by the `memories_tags_ai` / `_au` / `_ad` triggers, so the two
/// representations cannot drift.
///
/// The predicate stays in SQL rather than filtering parsed rows in Rust, which
/// is what lets `COUNT`, `LIMIT` and `OFFSET` stay correct — the reference hit
/// exactly that pagination bug as `DATA-02`. This previously scanned
/// `json_each(m.tags)` for the same reason, before `memory_tags` existed;
/// `idx_memory_tags_tag` now serves it without parsing JSON per row.
fn list_filters(input: &MemoryListInput) -> (String, Vec<Value>) {
    let mut conditions = vec!["m.deleted_at IS NULL".to_string()];
    let mut bindings: Vec<Value> = Vec::new();

    // Applied here rather than after the query so `COUNT`, `LIMIT` and
    // `OFFSET` all agree — the same reason every other filter in this function
    // is a SQL condition rather than a post-filter.
    if !input.include_sensitive {
        conditions.push("m.sensitive = 0".to_string());
    }

    if let Some(category) = input.category.as_ref().filter(|c| !c.is_empty()) {
        conditions.push("m.category = ?".to_string());
        bindings.push(Value::Text(category.clone()));
    }
    if let Some(source) = input.source.as_ref().filter(|s| !s.is_empty()) {
        conditions.push("m.source = ?".to_string());
        bindings.push(Value::Text(source.clone()));
    }
    for tag in input.tags.iter().flatten() {
        conditions.push(
            "EXISTS (SELECT 1 FROM memory_tags mt WHERE mt.memory_id = m.id AND mt.tag = ?)"
                .to_string(),
        );
        bindings.push(Value::Text(tag.clone()));
    }

    (format!("WHERE {}", conditions.join(" AND ")), bindings)
}

/// List memories newest-first, filtered by category, source and/or tags.
///
/// `limit` is clamped to [`LIST_LIMIT_MIN`]..=[`LIST_LIMIT_MAX`]; the clamped
/// value is echoed back in the result so callers can tell what was applied.
pub fn list_memories(conn: &Connection, input: &MemoryListInput) -> Result<MemoryListResult> {
    let limit = input.limit.clamp(LIST_LIMIT_MIN, LIST_LIMIT_MAX);
    let (where_clause, bindings) = list_filters(input);

    let total: i64 = conn.query_row(
        &format!("SELECT count(*) FROM memories m {}", where_clause),
        params_from_iter(bindings.iter()),
        |row| row.get(0),
    )?;

    let sql = format!(
        "SELECT {} FROM memories m {} ORDER BY m.created_at DESC, m.id DESC LIMIT ? OFFSET ?",
        MEMORY_COLUMNS, where_clause
    );
    let mut page_bindings = bindings;
    page_bindings.push(Value::Integer(limit as i64));
    page_bindings.push(Value::Integer(input.offset as i64));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(page_bindings.iter()), parse_memory_row)?;

    let mut memories = Vec::new();
    for row in rows {
        memories.push(row?);
    }

    Ok(MemoryListResult {
        count: memories.len(),
        memories,
        total: total.max(0) as usize,
        limit,
        offset: input.offset,
    })
}

/// Apply a partial update to a memory.
///
/// Deliberately does **not** touch `decay_rate`. An earlier version recomputed
/// it whenever `category` changed, which introduced a second writer: decay is
/// derived from `memory_type`, and [`reclassify_memories`] owns it. With both
/// writing, reclassifying a memory to `decision` and then editing its category
/// to `action_item` would silently contradict the classification. The reference
/// never touches `decay_rate` on update for exactly this reason.
///
/// `vitality`, `base_weight` and `access_count` are left alone too: they encode
/// accrued retrieval history, and resetting them on an edit would discard it.
pub fn update_memory(conn: &Connection, input: &MemoryUpdateInput) -> Result<UpdateOutcome> {
    if get_memory_by_id(conn, &input.memory_id)?.is_none() {
        return Ok(UpdateOutcome::NotFound);
    }

    let mut sets: Vec<&str> = Vec::new();
    let mut bindings: Vec<Value> = Vec::new();

    if let Some(content) = &input.content {
        sets.push("content = ?");
        bindings.push(Value::Text(content.clone()));
    }
    if let Some(category) = &input.category {
        sets.push("category = ?");
        bindings.push(Value::Text(category.clone()));
    }
    if let Some(tags) = &input.tags {
        sets.push("tags = ?");
        bindings.push(Value::Text(
            serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string()),
        ));
    }
    if let Some(metadata) = &input.metadata {
        sets.push("metadata = ?");
        bindings.push(Value::Text(
            serde_json::to_string(metadata).unwrap_or_else(|_| "{}".to_string()),
        ));
    }

    // `Option<bool>` rather than `bool`: `None` means "leave it alone", so an
    // update that does not mention the flag cannot silently clear it.
    if let Some(sensitive) = input.sensitive {
        sets.push("sensitive = ?");
        bindings.push(Value::Integer(sensitive as i64));
    }

    // Unconditional NULL rather than a bound parameter: the flag has only one
    // direction. Re-superseding is `remind_me_add`'s job, on detecting a
    // contradiction -- it is never something an update asserts directly.
    if input.clear_superseded {
        sets.push("superseded_by = NULL");
    }

    if sets.is_empty() {
        return Ok(UpdateOutcome::NoFields);
    }

    // Before the UPDATE, not after: a revision exists to hold the value this
    // edit is about to replace. Issue #109's audit found the reference writes
    // revisions from this path alone — reclassify, normalize, annotate,
    // consolidate and decompose record nothing — so this is the only call
    // site, and `history.rs`'s module docs carry the reasoning.
    //
    // Access tracking never reaches here (it writes `accessed_at` directly),
    // so a read cannot produce a revision.
    let tracked = crate::history::TrackedChanges {
        content: input.content.clone(),
        category: input.category.clone(),
        tags_json: input
            .tags
            .as_ref()
            .map(|t| serde_json::to_string(t).unwrap_or_else(|_| "[]".to_string())),
        metadata_json: input
            .metadata
            .as_ref()
            .map(|m| serde_json::to_string(m).unwrap_or_else(|_| "{}".to_string())),
        sensitive: input.sensitive,
    };
    crate::history::capture_revision(conn, &input.memory_id, &tracked, None)?;

    sets.push("updated_at = ?");
    bindings.push(Value::Text(Utc::now().to_rfc3339()));
    bindings.push(Value::Text(input.memory_id.clone()));

    conn.execute(
        &format!("UPDATE memories SET {} WHERE id = ?", sets.join(", ")),
        params_from_iter(bindings.iter()),
    )?;

    // Only a content change invalidates the stored embeddings — category,
    // tags and metadata don't change what the text means. Best-effort, same
    // as `add_memory`: an embedding failure here does not undo the update
    // that already committed.
    if let Some(content) = &input.content {
        if let Some(embedder) = crate::embedder::available_embedder() {
            let _ = crate::vectors::embed_and_store(conn, &*embedder, &input.memory_id, content);
        }
    }

    let memory =
        get_memory_by_id(conn, &input.memory_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    crate::events::emit(crate::events::Event::Updated, &memory.id, &memory.category);

    Ok(UpdateOutcome::Updated(Box::new(memory)))
}

/// Delete a memory. Returns `false` if no live memory had that id.
///
/// Soft-deletes (tombstones via `deleted_at` + bumps `updated_at`) when sync
/// is configured (`NODE_ID and HUB_URL and SYNC_SECRET`, `#57`), matching the
/// reference exactly: a hard `DELETE` produces no outbox row at all (the
/// sync triggers only fire on INSERT/UPDATE), so it would otherwise silently
/// resurrect on the next pull elsewhere. The tombstone is excluded from every
/// normal read (`deleted_at IS NULL` everywhere this crate reads memories)
/// and, on a node with sync disabled, there is nothing to propagate to, so
/// this is a plain, immediate delete exactly as before.
///
/// **Known limitation**: the installed `memories_outbox_au` trigger's
/// payload (`schema_triggers.sql`, generated verbatim and not this crate's
/// file to hand-edit) does not carry `deleted_at` at all, so this tombstone
/// does not yet actually propagate over the wire even though the row is
/// correctly marked locally — see
/// `docs/adr/0004-sync-protocol-and-conflict-resolution.md`'s "Known
/// limitation" section. There is no background compaction of old tombstones
/// yet either (the reference's own `TOMBSTONE_RETENTION_DAYS`); both are
/// left for a follow-up once the schema carries what tombstone propagation
/// actually needs.
///
/// The FTS row and `memory_tags` are handled by triggers. Everything else is
/// cleaned up explicitly, because the reference's schema carries **no foreign
/// keys** on `memory_entities`, `memory_feedback` or `memory_associations` —
/// deliberately, since sync can deliver a link before the memory it points at,
/// and a cascade would reject that. This crate previously relied on a cascade
/// it had added itself; regenerating the schema from `remind_me` removed it.
pub fn delete_memory(conn: &Connection, memory_id: &str) -> Result<bool> {
    // Fetched before either delete path: once a hard delete removes the row
    // there is no `WHERE id = ?` left to find its rowid by, and that rowid
    // is exactly what `vec_chunks` is keyed on. Matches the reference's own
    // `memory_delete` order (fetch rowid, clean up vectors, then delete or
    // tombstone) rather than only cleaning up vectors on a hard delete —
    // a tombstoned memory's embeddings are stale the moment it stops being
    // searchable, same as an incoming sync tombstone's are.
    // The category comes out with the rowid rather than in a second query: a
    // hard delete removes the row, so after this point there is nothing left
    // to read it from, and the event would have to guess.
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT rowid, category FROM memories WHERE id = ? AND deleted_at IS NULL",
            params![memory_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let memory_rowid: Option<i64> = existing.as_ref().map(|(rowid, _)| *rowid);
    let category: Option<String> = existing.map(|(_, category)| category);
    if memory_rowid.is_none() {
        return Ok(false);
    }

    // SQLite reuses freed rowids: left alone, a later memory landing on this
    // same rowid would silently inherit these chunk vectors through the
    // surviving `vec_chunks` rows.
    if let Some(memory_rowid) = memory_rowid {
        crate::vectors::delete_chunks_for_memory(conn, memory_rowid)?;
    }

    let affected = if crate::sync::sync_enabled() {
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE memories SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
            params![now, now, memory_id],
        )?
    } else {
        conn.execute(
            "DELETE FROM memories WHERE id = ? AND deleted_at IS NULL",
            params![memory_id],
        )?
    };
    if affected == 0 {
        return Ok(false);
    }

    // Entities themselves survive — other memories may still mention them.
    for table in ["memory_entities", "memory_feedback"] {
        conn.execute(
            &format!("DELETE FROM {} WHERE memory_id = ?", table),
            params![memory_id],
        )?;
    }
    conn.execute(
        "DELETE FROM memory_associations WHERE memory_id_a = ? OR memory_id_b = ?",
        params![memory_id, memory_id],
    )?;

    crate::events::emit(
        crate::events::Event::Deleted,
        memory_id,
        category.as_deref().unwrap_or(""),
    );

    Ok(true)
}

/// Delete several memories by id in one request.
///
/// Applies the exact same per-memory logic as [`delete_memory`] to each id
/// independently — reused, not reimplemented, so the two paths cannot drift —
/// and one missing id does not fail the rest of the batch.
pub fn bulk_delete(conn: &Connection, ids: &[String]) -> Result<BulkDeleteResult> {
    let mut result = BulkDeleteResult::default();
    for id in ids {
        if delete_memory(conn, id)? {
            result.deleted.push(id.clone());
        } else {
            result.not_found.push(id.clone());
        }
    }
    Ok(result)
}

/// Add, remove, or replace tags on several memories in one request.
///
/// A missing id is recorded in `not_found` and the rest of the batch still
/// applies, matching [`bulk_delete`]'s per-item error handling.
pub fn bulk_tag(conn: &Connection, input: &BulkTagInput) -> Result<BulkTagResult> {
    let now = Utc::now().to_rfc3339();
    let mut result = BulkTagResult::default();

    for id in &input.ids {
        let Some(memory) = get_memory_by_id(conn, id)? else {
            result.not_found.push(id.clone());
            continue;
        };

        let new_tags = match input.mode {
            TagMode::Set => crate::entity::dedup_preserving_order(input.tags.iter().cloned()),
            TagMode::Remove => memory
                .tags
                .into_iter()
                .filter(|t| !input.tags.contains(t))
                .collect(),
            TagMode::Add => crate::entity::dedup_preserving_order(
                memory.tags.into_iter().chain(input.tags.iter().cloned()),
            ),
        };

        conn.execute(
            "UPDATE memories SET tags = ?, updated_at = ? WHERE id = ?",
            params![
                serde_json::to_string(&new_tags).unwrap_or_else(|_| "[]".to_string()),
                now,
                id
            ],
        )?;
        result.updated.push(id.clone());
    }

    Ok(result)
}

/// Apply a batch of annotations: SPO triple fields and entity mentions.
///
/// Per-item error handling, matching the reference: an unknown `memory_id` is
/// recorded in `errors` and the rest of the batch still applies. An
/// all-or-nothing transaction would mean one stale id from an extraction pass
/// discards up to 99 good annotations.
///
/// Only the SPO fields actually supplied are written; omitted ones keep their
/// current value. `updated_at` moves whenever an annotation is applied, even if
/// it only added entity mentions.
pub fn annotate_memories(conn: &Connection, input: &AnnotateInput) -> Result<AnnotateResult> {
    let now = Utc::now().to_rfc3339();
    let mut results = Vec::new();
    let mut errors = Vec::new();

    for annotation in &input.annotations {
        if get_memory_by_id(conn, &annotation.memory_id)?.is_none() {
            errors.push(AnnotationError {
                memory_id: annotation.memory_id.clone(),
                error: "memory not found".to_string(),
            });
            continue;
        }

        let mut sets: Vec<&str> = Vec::new();
        let mut bindings: Vec<Value> = Vec::new();
        for (column, value) in [
            ("subject = ?", &annotation.subject),
            ("predicate = ?", &annotation.predicate),
            ("object = ?", &annotation.object),
        ] {
            if let Some(v) = value {
                sets.push(column);
                bindings.push(Value::Text(v.clone()));
            }
        }

        sets.push("updated_at = ?");
        bindings.push(Value::Text(now.clone()));
        bindings.push(Value::Text(annotation.memory_id.clone()));

        conn.execute(
            &format!("UPDATE memories SET {} WHERE id = ?", sets.join(", ")),
            params_from_iter(bindings.iter()),
        )?;

        let entities_linked = crate::entity::apply_entity_mentions(
            conn,
            &annotation.memory_id,
            &annotation.entities,
        )?;

        // After the mentions, never before: the edge is only recorded when both
        // sides of the triple resolve to *known* entities, and the ones this
        // annotation names were created a moment ago.
        crate::entity::maybe_link_entity_relation(
            conn,
            annotation.subject.as_deref(),
            annotation.predicate.as_deref(),
            annotation.object.as_deref(),
        )?;

        results.push(AnnotationApplied {
            memory_id: annotation.memory_id.clone(),
            entities_linked,
        });
    }

    Ok(AnnotateResult {
        annotated: results.len(),
        results,
        errors,
    })
}

/// Apply memory-type classifications, updating the decay rate to match.
///
/// `decay_rate` is a pure function of `memory_type` ([`get_decay_rate`]), and
/// this is the only place that writes it. Idempotent: reclassifying overwrites
/// the previous type and rate.
///
/// Unknown ids are collected into `not_found` rather than failing the batch —
/// a classification pass over stale ids should not discard its good work.
/// Vitality and `base_weight` are untouched; classification says what a memory
/// *is*, not how much it has been used.
pub fn reclassify_memories(conn: &Connection, input: &ReclassifyInput) -> Result<ReclassifyResult> {
    let now = Utc::now().to_rfc3339();
    let mut updated = 0;
    let mut not_found = Vec::new();

    for classification in &input.classifications {
        if get_memory_by_id(conn, &classification.memory_id)?.is_none() {
            not_found.push(classification.memory_id.clone());
            continue;
        }

        conn.execute(
            "UPDATE memories SET memory_type = ?, decay_rate = ?, updated_at = ? WHERE id = ?",
            params![
                classification.memory_type,
                get_decay_rate(&classification.memory_type),
                now,
                classification.memory_id
            ],
        )?;
        updated += 1;
    }

    Ok(ReclassifyResult {
        updated,
        not_found,
        total: input.classifications.len(),
    })
}

/// Fetch memories still awaiting classification, with a snippet for review.
///
/// `total_unclassified` counts every remaining memory, not just this page, so a
/// caller can tell whether another round is worth requesting.
pub fn unclassified_batch(
    conn: &Connection,
    input: &ReclassifyBatchInput,
) -> Result<ReclassifyBatchResult> {
    let batch_size = input
        .batch_size
        .clamp(RECLASSIFY_BATCH_MIN, RECLASSIFY_BATCH_MAX);

    let total_unclassified: i64 = conn.query_row(
        "SELECT count(*) FROM memories WHERE memory_type = ? AND deleted_at IS NULL",
        params![UNCLASSIFIED],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT id, substr(content, 1, 500), category, tags
           FROM memories
          WHERE memory_type = ? AND deleted_at IS NULL
          ORDER BY created_at, id
          LIMIT ?",
    )?;
    let rows = stmt.query_map(params![UNCLASSIFIED, batch_size as i64], |row| {
        let tags_json: String = row.get(3)?;
        Ok(UnclassifiedMemory {
            id: row.get(0)?,
            content_snippet: row.get(1)?,
            category: row.get(2)?,
            tags: serde_json::from_str(&tags_json).unwrap_or_default(),
        })
    })?;

    let mut memories = Vec::new();
    for row in rows {
        memories.push(row?);
    }

    Ok(ReclassifyBatchResult {
        memories,
        total_unclassified: total_unclassified.max(0) as usize,
    })
}

/// Effective vitality as a SQL expression over the alias `m`.
///
/// The stored `vitality` column is a write-time snapshot that never decays, so
/// filtering on it means `include_dormant: false` filters nothing and
/// `min_vitality` compares against a number unrelated to the memory's current
/// standing. This calls [`vitality::register_sql_functions`]'s scalar function
/// instead, which keeps the predicate *before* `LIMIT`.
///
/// Doing it in Rust after the fetch would push the filter after the limit, the
/// shape the reference warns about as `DI-03`: the page would be truncated
/// first and then thinned, under-filling every result set.
///
/// `coalesce`s `accessed_at` to `created_at`, because a memory that has never
/// been retrieved should age from when it was written.
fn effective_vitality_sql() -> String {
    format!(
        "{}(m.base_weight, m.access_count, m.decay_rate, coalesce(m.accessed_at, m.created_at))",
        EFFECTIVE_VITALITY_FN
    )
}

/// Ids of every memory currently marked sensitive.
///
/// One query rather than a per-result check: the set is small by construction
/// — a flag a user sets deliberately, on a single-user store — and the callers
/// need membership tests, not rows.
fn sensitive_ids(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT id FROM memories WHERE sensitive = 1")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<std::collections::HashSet<String>>>()?;
    Ok(ids)
}

/// Search with the configured embedder, if one is resolvable and answering.
///
/// Thin wrapper over [`search_memories_with_embedder`]. Every existing caller
/// keeps its behaviour: `available_embedder` probes the configured backend and
/// yields `None` when none is set or it does not answer, which is the same
/// "keyword-only" case the fused ranker already handles.
pub fn search_memories(
    conn: &Connection,
    input: &MemorySearchInput,
) -> Result<Vec<MemorySearchResult>> {
    let configured = crate::embedder::available_embedder();
    search_memories_with_embedder(
        conn,
        input,
        configured
            .as_ref()
            .map(|e| &**e as &dyn crate::embedder::Embedder),
    )
}

/// The plain-`Vec` form, for the many callers that want results and not the
/// budget accounting. Kept so #200 did not have to churn ten call sites to
/// report five numbers.
pub fn search_memories_with_embedder(
    conn: &Connection,
    input: &MemorySearchInput,
    embedder: Option<&dyn crate::embedder::Embedder>,
) -> Result<Vec<MemorySearchResult>> {
    Ok(search_memories_budgeted(conn, input, embedder)?.results)
}

/// Search with a caller-supplied embedder, or none.
///
/// [`Embedder`](crate::embedder::Embedder) exists as a trait so "a future
/// backend (ONNX-in-process, say) can be added without touching anything that
/// already depends on this" — its own words. That was not quite true while this
/// function reached for the concrete `available_embedder()` itself: a caller
/// holding a perfectly good `impl Embedder` had no way to get it in here, and
/// the only supported backend was the one this module named.
///
/// `None` means keyword-only, explicitly rather than by a failed probe. That
/// distinction matters to a caller that needs the *same* query to return the
/// *same* rows every time: `search_memories` degrades to keyword-only when the
/// daemon is unreachable, which is the right call for an interactive search and
/// the wrong one where reproducibility is the requirement. Passing `None` — or a
/// deterministic in-process embedder — makes that choice at the call site rather
/// than leaving it to whether a probe happened to succeed.
/// [`search_memories_with_embedder`], but keeping the token-budget counts
/// instead of discarding them (#200).
pub fn search_memories_budgeted(
    conn: &Connection,
    input: &MemorySearchInput,
    embedder: Option<&dyn crate::embedder::Embedder>,
) -> Result<crate::retrieval::TrimOutcome> {
    // Reading the deadline here rather than inside means the clock starts
    // before any work — including the FTS query, which is not free on a large
    // store.
    search_memories_deadlined(
        conn,
        input,
        embedder,
        crate::retrieval::Deadline::from_env(),
    )
}

/// [`search_memories_budgeted`] with the wall-clock deadline supplied rather
/// than read from the environment (#257).
///
/// Exists for the same reason [`search_memories_with_embedder`] does: the
/// environment-reading wrapper is convenient and untestable. A deadline from
/// `REMIND_ME_SEARCH_DEADLINE_MS` starts its clock when the search starts, so
/// from outside there is no way to arrange for it to have already passed
/// except by making the search itself slow — which is a race, not a test.
///
/// Also useful in production: a background saved-search poll can afford a
/// longer budget than an interactive query, and passing one beats mutating a
/// process-global.
pub fn search_memories_deadlined(
    conn: &Connection,
    input: &MemorySearchInput,
    embedder: Option<&dyn crate::embedder::Embedder>,
    deadline: crate::retrieval::Deadline,
) -> Result<crate::retrieval::TrimOutcome> {
    let mut timing = crate::retrieval::SearchTiming {
        deadline_ms: deadline.limit_ms(),
        ..Default::default()
    };

    let weights = choose_rrf_weights(&input.query, input.strategy);

    // Raw user text is not a valid FTS5 MATCH expression — ordinary punctuation
    // is operator syntax there, so a question like "what's the plan?" was a
    // syntax error rather than a search. An empty result means nothing was
    // searchable; MATCH on an empty string is itself an error.
    let match_expr = sanitize_fts_query(&input.query);
    if match_expr.is_empty() {
        let mut outcome = trim_by_token_budget(Vec::new(), input.token_budget);
        timing.elapsed_ms = deadline.elapsed_ms();
        outcome.timing = timing;
        return Ok(outcome);
    }

    // Derived from MEMORY_COLUMNS rather than spelled out, so adding a column
    // cannot leave this query selecting a stale subset — which is exactly how
    // `base_weight` slipped past here once. `bm25_score` rides along as a
    // trailing extra column -- `parse_memory_row` only ever looks up columns
    // by name, so it ignores it, and `RrfFusion::Score` mode needs the raw
    // magnitude alongside the memory it belongs to.
    let mut sql = format!(
        "SELECT {}, bm25(memories_fts) AS bm25_score
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ? AND m.superseded_by IS NULL AND m.deleted_at IS NULL",
        prefixed_memory_columns("m")
    );

    let mut bindings: Vec<Value> = vec![Value::Text(match_expr.clone())];

    let effective = effective_vitality_sql();
    if !input.include_dormant {
        sql.push_str(&format!(" AND {} >= {}", effective, VITALITY_FLOOR));
    }
    if input.min_vitality > 0.0 {
        sql.push_str(&format!(" AND {} >= {}", effective, input.min_vitality));
    }
    if let Some(ref cat) = input.category {
        sql.push_str(" AND m.category = ?");
        bindings.push(Value::Text(cat.clone()));
    }
    if !input.include_sensitive {
        sql.push_str(" AND m.sensitive = 0");
    }

    sql.push_str(" ORDER BY bm25(memories_fts) LIMIT ?");
    bindings.push(Value::Integer((input.limit * 2) as i64));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(bindings.iter()), |row| {
        let memory = parse_memory_row(row)?;
        let bm25_score: f64 = row.get("bm25_score")?;
        Ok((memory, bm25_score))
    })?;

    let mut keyword_memories = Vec::new();
    let mut keyword_bm25 = std::collections::HashMap::new();
    for r in rows {
        let (memory, bm25_score) = r?;
        keyword_bm25.insert(memory.id.clone(), bm25_score);
        keyword_memories.push(memory);
    }

    // Semantic augmentation is entirely optional: no embedder passed in — because
    // the caller has none, or because `search_memories` found none configured or
    // reachable — means an empty list, which `rank_rrf` treats as "semantic
    // search did not run" rather than as a real empty result — see its own
    // doc comment. Any failure during the search itself (the embedder
    // rejects the query text, say) degrades the same way rather than
    // failing the keyword search it would otherwise still have been able to
    // answer.
    // The deadline gates *entry* to the semantic stage. Once inside, the
    // embedder's own socket timeouts bound it and this cannot preempt them —
    // see `Deadline`'s doc comment. What this prevents is a search that has
    // already spent its budget on the keyword half going on to spend a
    // connect-timeout more on a daemon that is not answering.
    //
    // Degrading to keyword-only rather than erroring is the same choice the
    // `None` arm below already makes for an unconfigured embedder: partial
    // results with an honest label beat a killed query.
    let embedder = match embedder {
        Some(e) if deadline.expired() => {
            timing.skipped.push("semantic".to_string());
            let _ = e;
            None
        }
        other => other,
    };

    let (semantic_memories, semantic_similarity) = match embedder {
        Some(embedder) => {
            let extra_texts = crate::query_expansion::expand_query(&input.query);
            let scored = crate::vectors::semantic_search_scored(
                conn,
                embedder,
                &input.query,
                &extra_texts,
                input.limit * 2,
                input.category.as_deref(),
            )
            .unwrap_or_default();
            let mut memories = Vec::with_capacity(scored.len());
            let mut similarity = std::collections::HashMap::with_capacity(scored.len());
            for (memory, sim) in scored {
                similarity.insert(memory.id.clone(), sim as f64);
                memories.push(memory);
            }
            // Filtering the keyword SQL alone would not be enough: RRF fuses
            // two independent result sets, so a sensitive memory excluded from
            // the keyword half could still arrive through the semantic half and
            // rank into the output. Done here rather than by threading a
            // parameter through `semantic_search_scored`, which would change an
            // existing public signature.
            if !input.include_sensitive {
                let hidden = sensitive_ids(conn)?;
                memories.retain(|m| !hidden.contains(&m.id));
                similarity.retain(|id, _| !hidden.contains(id));
            }
            (memories, similarity)
        }
        None => (Vec::new(), std::collections::HashMap::new()),
    };

    let config = RrfConfig {
        k: rrf_k_from_env(),
        weights,
        fusion: RrfFusion::from_env(),
    };
    let signals = RrfSignals {
        keyword_bm25,
        semantic_similarity,
    };

    let ranked = rank_rrf(keyword_memories, semantic_memories, config, &signals);

    // Query-contextual feedback adjustment (issue #94): nudges `score` by
    // any similarly-worded past feedback before truncating to `limit`, so a
    // memory boosted from just past the cutoff can still make the page.
    let mut ranked = apply_feedback_adjustment(conn, &input.query, ranked)?;

    // Optional cross-encoder rerank of the head, before truncation. The pool
    // is deliberately wider than `limit`: rescoring only what was already
    // going to be returned would throw away the most useful thing a
    // cross-encoder does, which is promote a candidate from just past the
    // cutoff. Feedback runs first so its nudge perturbs the order feeding the
    // cross-encoder, leaving the cross-encoder with the final say.
    ranked.truncate(crate::reranker::pool_size(input.limit));
    // Second gate. The reranker is in-process rather than networked, so this
    // is not about a hung daemon — it is about not spending cross-encoder CPU
    // reordering results for a caller who has already waited longer than they
    // agreed to. The results stay in RRF order, which is a worse ranking, not
    // a wrong one.
    //
    // Guarded on **both** `available()` and `enabled()`, which mean different
    // things: `enabled()` is the setting and defaults to *true*, while
    // `available()` is `cfg!(feature = "rerank")` and is false on a default
    // build. Guarding on `enabled()` alone would report "skipped rerank" on
    // every expired-deadline search in a build with no reranker compiled in —
    // a fabricated degradation signal, and precisely the failure this line
    // exists to avoid.
    let rerank_would_run = crate::reranker::available() && crate::reranker::enabled();
    let mut ranked = if rerank_would_run && deadline.expired() {
        timing.skipped.push("rerank".to_string());
        ranked
    } else {
        crate::reranker::maybe_rerank(&input.query, ranked)
    };
    ranked.truncate(input.limit);

    let mut outcome = trim_by_token_budget(ranked, input.token_budget);
    timing.elapsed_ms = deadline.elapsed_ms();
    outcome.timing = timing;
    Ok(outcome)
}

/// Category/tag conditions shared by both branches of [`search_paginated`].
///
/// Deliberately narrower than [`list_filters`]: the reference's `api_search`
/// supports `category` and `tags`, not `source` — this mirrors that exactly
/// rather than silently offering a superset.
fn search_page_filters(input: &SearchPageInput) -> (String, Vec<Value>) {
    let mut sql = String::new();
    let mut bindings: Vec<Value> = Vec::new();

    if let Some(category) = input.category.as_ref().filter(|c| !c.is_empty()) {
        sql.push_str(" AND m.category = ?");
        bindings.push(Value::Text(category.clone()));
    }
    for tag in input.tags.iter().flatten() {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM memory_tags mt WHERE mt.memory_id = m.id AND mt.tag = ?)",
        );
        bindings.push(Value::Text(tag.clone()));
    }

    (sql, bindings)
}

/// Paginated full-text search behind `GET /api/memories/search`.
///
/// Distinct from [`search_memories`]: that function serves the MCP tool's
/// ranked, token-budgeted response; this one serves a `total`/`has_more`
/// pagination envelope, matching [`list_memories`]'s shape so a client pages
/// through search results the same way it pages through a list.
///
/// `input.entity` — an entity name already extracted from the query text via
/// [`crate::fts::extract_entity_token`] — narrows results to memories linked
/// to that entity (via `memory_entities`) or whose structured subject/object
/// equals its canonical name (`FT-04`). An entity name this store has never
/// seen is not an error: it is a real empty page, reported with `message`
/// rather than a 404, since the free-text portion of the query (if any) was
/// still a well-formed request.
///
/// With no free text left after stripping the `entity:` token, matching
/// memories are listed newest-first instead of FTS-ranked — there is nothing
/// for BM25 to rank.
///
/// Superseded memories are excluded unconditionally in both branches. The
/// reference only applies that exclusion on the entity-scoped path (its
/// plain-FTS branch has no `superseded_by` filter at all); this crate's
/// non-paginated `search_memories` has excluded superseded rows unconditionally
/// since the dormancy-filtering fix, and reproducing the reference's
/// inconsistency here would mean two search entry points disagreeing about
/// whether a stale, superseded chunk is a result.
pub fn search_paginated(conn: &Connection, input: &SearchPageInput) -> Result<SearchPageResult> {
    let limit = input.limit.clamp(LIST_LIMIT_MIN, LIST_LIMIT_MAX);
    let offset = input.offset;
    let (filter_sql, filter_bindings) = search_page_filters(input);

    let mut entity_sql = String::new();
    let mut entity_bindings: Vec<Value> = Vec::new();
    if let Some(entity_query) = &input.entity {
        let Some(entity) = crate::entity::resolve_entity(conn, entity_query)? else {
            return Ok(SearchPageResult {
                total: 0,
                count: 0,
                offset,
                limit,
                has_more: false,
                memories: Vec::new(),
                message: Some(format!("No entity found matching {:?}.", entity_query)),
            });
        };
        let canonical = crate::entity::normalize_entity_name(&entity.name);
        entity_sql.push_str(
            " AND (EXISTS (SELECT 1 FROM memory_entities me \
               WHERE me.memory_id = m.id AND me.entity_id = ?) \
               OR lower(m.subject) = ? OR lower(m.object) = ?)",
        );
        entity_bindings = vec![
            Value::Text(entity.id),
            Value::Text(canonical.clone()),
            Value::Text(canonical),
        ];
    }

    let match_expr = sanitize_fts_query(&input.query);

    let (total, memories) = if !match_expr.is_empty() {
        let mut bindings = vec![Value::Text(match_expr)];
        bindings.extend(filter_bindings.iter().cloned());
        bindings.extend(entity_bindings.iter().cloned());

        let total: i64 = conn.query_row(
            &format!(
                "SELECT count(*) FROM memories m
                   JOIN memories_fts fts ON m.rowid = fts.rowid
                  WHERE memories_fts MATCH ? AND m.superseded_by IS NULL
                    AND m.deleted_at IS NULL{}{}",
                filter_sql, entity_sql
            ),
            params_from_iter(bindings.iter()),
            |row| row.get(0),
        )?;

        let mut page_bindings = bindings;
        page_bindings.push(Value::Integer(limit as i64));
        page_bindings.push(Value::Integer(offset as i64));
        let sql = format!(
            "SELECT {}
               FROM memories m JOIN memories_fts fts ON m.rowid = fts.rowid
              WHERE memories_fts MATCH ? AND m.superseded_by IS NULL AND m.deleted_at IS NULL{}{}
              ORDER BY bm25(memories_fts) LIMIT ? OFFSET ?",
            prefixed_memory_columns("m"),
            filter_sql,
            entity_sql
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(page_bindings.iter()), parse_memory_row)?;
        let mut memories = Vec::new();
        for row in rows {
            memories.push(row?);
        }
        (total.max(0) as usize, memories)
    } else {
        let mut bindings = filter_bindings;
        bindings.extend(entity_bindings.iter().cloned());

        let total: i64 = conn.query_row(
            &format!(
                "SELECT count(*) FROM memories m
                  WHERE m.superseded_by IS NULL AND m.deleted_at IS NULL{}{}",
                filter_sql, entity_sql
            ),
            params_from_iter(bindings.iter()),
            |row| row.get(0),
        )?;

        let mut page_bindings = bindings;
        page_bindings.push(Value::Integer(limit as i64));
        page_bindings.push(Value::Integer(offset as i64));
        let sql = format!(
            "SELECT {} FROM memories m
              WHERE m.superseded_by IS NULL AND m.deleted_at IS NULL{}{}
              ORDER BY m.created_at DESC LIMIT ? OFFSET ?",
            prefixed_memory_columns("m"),
            filter_sql,
            entity_sql
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(page_bindings.iter()), parse_memory_row)?;
        let mut memories = Vec::new();
        for row in rows {
            memories.push(row?);
        }
        (total.max(0) as usize, memories)
    };

    let count = memories.len();
    Ok(SearchPageResult {
        total,
        count,
        offset,
        limit,
        has_more: total > offset + count,
        memories,
        message: None,
    })
}

/// Search, reinforce co-retrieval, and attach whichever expansions were asked
/// for.
///
/// Co-retrieval is recorded on **every** search that returns two or more
/// results, whether or not `expand_co_retrieval` is set — surfacing is opt-in,
/// recording is not. This does mean a search mutates, which is why the write is
/// confined to this wrapper: [`search_memories`] stays a pure read for callers
/// that only want results.
///
/// The three expansion sections sit *outside* the ranked list and never merge
/// into it, so they do not consume `limit`. See [`crate::expansion`] for why
/// keeping co-retrieval out of the ranking matters.
pub fn search_with_expansions(
    conn: &Connection,
    input: &MemorySearchInput,
) -> Result<MemorySearchResponse> {
    let configured = crate::embedder::available_embedder();

    // The bootstrap is assembled first because it *spends* from the same
    // budget, and the hits are then searched against what is left. Doing it
    // the other way round — search on the full budget, then prepend — would
    // put the response over budget by exactly the size of the bootstrap,
    // which is the failure #200 spent an issue making visible.
    let bootstrap = if input.bootstrap {
        Some(crate::promotion::bootstrap(conn, input.token_budget)?)
    } else {
        None
    };

    // Deliberately *not* shadowing `input`: the response reports the budget
    // the caller set, and reading it off a locally-reduced copy would quietly
    // report 600 to someone who asked for 800 and got a 200-token bootstrap.
    //
    // `saturating_sub` rather than a bare `-`: the reserve is capped at half
    // the budget so this cannot currently go negative, but a future change to
    // the cap should degrade to "no budget left for hits" rather than wrap to
    // an enormous one. A budget of 0 (unlimited) stays 0 here, which is right
    // — an unlimited search does not become limited by having a bootstrap.
    let spent = bootstrap.as_ref().map_or(0, |b| b.tokens_used);
    let hit_input;
    let hit_input = if spent == 0 {
        input
    } else {
        hit_input = MemorySearchInput {
            token_budget: input.token_budget.saturating_sub(spent),
            ..input.clone()
        };
        &hit_input
    };

    let outcome = search_memories_budgeted(
        conn,
        hit_input,
        configured
            .as_ref()
            .map(|e| &**e as &dyn crate::embedder::Embedder),
    )?;
    let memories = outcome.results;
    let ids: Vec<String> = memories.iter().map(|r| r.memory.id.clone()).collect();

    expansion::record_co_retrieval(conn, &ids)?;

    // Direct hits only. Expansion results are a discovery aid surfaced by
    // adjacency, not answers to the query, and recording them would inflate
    // the vitality of every neighbour on every expanded search.
    //
    // Ordered after the expansions are built so they read the pre-access state:
    // recording rewrites `vitality` and `accessed_at`, and an expansion should
    // describe the store as the caller found it.
    let response = MemorySearchResponse {
        related_via_entities: if input.expand_entities {
            Some(expansion::expand_via_entities(conn, &ids)?)
        } else {
            None
        },
        related_via_neighbors: if input.include_neighbors {
            Some(expansion::expand_via_neighbors(conn, &memories)?)
        } else {
            None
        },
        related_via_co_retrieval: if input.expand_co_retrieval {
            Some(expansion::expand_via_co_retrieval(conn, &ids)?)
        } else {
            None
        },
        timing: outcome.timing,
        bootstrap,
        memories,
        total_candidates: outcome.total_candidates,
        returned: outcome.returned,
        trimmed: outcome.trimmed,
        // Hits only, matching this field's documented meaning. The bootstrap
        // carries its own `tokens_used`, so the response total is the sum of
        // the two rather than either one restated — see the renderer, which
        // prints both.
        tokens_used: outcome.tokens_used,
        // The budget the *caller* set, not what was left after the reserve.
        budget: input.token_budget,
    };

    crate::vitality::record_accesses(conn, &ids)?;

    Ok(response)
}

/// Which memories still need a triple or entity mentions.
///
/// A memory qualifies when it is live, is **not a raw dialog**, has no SPO
/// triple at all, and has no entity links at all.
///
/// Two parts of that are easy to get wrong. The `dialog` exclusion is not
/// cosmetic: a captured transcript's facts are meant to come out through
/// `decompose`, so without it every captured conversation would flood this
/// backlog. And a memory needs to be missing *both* signals — one that has
/// entities but no triple is already considered annotated, so an `OR` here
/// would keep re-offering work that is done.
///
/// `skeleton` is excluded on the same grounds and then some (#207): it is
/// Mermaid source describing a conversation's shape, so there is no fact in it
/// to extract and offering one costs a model call to find that out.
fn unannotated_where() -> &'static str {
    "m.superseded_by IS NULL
     AND m.deleted_at IS NULL
     AND m.category NOT IN ('dialog', 'skeleton')
     AND m.subject IS NULL AND m.predicate IS NULL AND m.object IS NULL
     AND NOT EXISTS (SELECT 1 FROM memory_entities me WHERE me.memory_id = m.id)"
}

/// A page of memories awaiting extraction, newest first.
///
/// The read half of the annotation loop: `remind_me_annotate` writes triples
/// and mentions, and this is what tells a caller which memories still need
/// them. Without it, annotation could only be applied to memories the caller
/// already happened to know about.
pub fn unannotated_batch(
    conn: &Connection,
    input: &ExtractBatchInput,
) -> Result<ExtractBatchResult> {
    let batch_size = input.batch_size.clamp(EXTRACT_BATCH_MIN, EXTRACT_BATCH_MAX);
    let predicate = unannotated_where();

    let total: i64 = conn.query_row(
        &format!("SELECT count(*) FROM memories m WHERE {}", predicate),
        [],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(&format!(
        "SELECT m.id, m.content, m.category, m.memory_type, m.tags
           FROM memories m
          WHERE {}
          ORDER BY m.created_at DESC
          LIMIT ?",
        predicate
    ))?;
    let memories: Vec<UnannotatedMemory> = stmt
        .query_map(params![batch_size as i64], |row| {
            let content: String = row.get("content")?;
            let tags_json: String = row.get("tags")?;
            Ok(UnannotatedMemory {
                id: row.get("id")?,
                // By characters, not bytes — a multi-byte character on the
                // boundary would panic a byte slice.
                content_snippet: content.chars().take(500).collect(),
                category: row.get("category")?,
                memory_type: row.get("memory_type")?,
                tags: serde_json::from_str(&tags_json).unwrap_or_default(),
            })
        })?
        .collect::<Result<_>>()?;

    Ok(ExtractBatchResult {
        memories,
        total_unannotated: total.max(0) as usize,
    })
}
