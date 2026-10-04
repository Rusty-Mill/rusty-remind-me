use super::{Result, Store, StoreError};
use crate::db::memories::{
    EntityScope, KeywordFilter, ListFilter, Memories, MemoryEdit, NewMemory, PageFilter,
};
use crate::expansion::{self, MemorySearchResponse};
use crate::kinds::StructuredFields;
use crate::models::{
    AnnotateInput, AnnotateResult, AnnotationApplied, AnnotationError, BulkDeleteResult,
    BulkTagInput, BulkTagResult, ExtractBatchInput, ExtractBatchResult, Memory, MemoryAddInput,
    MemoryListInput, MemoryListResult, MemorySearchInput, MemorySearchResult, MemoryUpdateInput,
    ReclassifyBatchInput, ReclassifyBatchResult, ReclassifyInput, ReclassifyResult,
    SearchPageInput, SearchPageResult, TagMode, UpdateOutcome, EXTRACT_BATCH_MAX,
    EXTRACT_BATCH_MIN, LIST_LIMIT_MAX, LIST_LIMIT_MIN, RECLASSIFY_BATCH_MAX, RECLASSIFY_BATCH_MIN,
    UNCLASSIFIED,
};
use crate::retrieval::{
    choose_rrf_weights, rank_rrf, rrf_k_from_env, trim_by_token_budget, RrfConfig, RrfFusion,
    RrfSignals,
};
use crate::vitality::{
    apply_feedback_adjustment, calculate_vitality, get_decay_rate, get_source_prior,
    get_type_prior, VITALITY_FLOOR,
};
use chrono::Utc;

pub fn add_memory(store: &Store<'_>, input: MemoryAddInput) -> Result<Memory> {
    add_memory_with(store, input, &StructuredFields::default())
}

/// [`add_memory`] with the structured fields (`memory_type`, `confidence`,
/// the validity window, `outcome`) a writer may set. Validated first: a
/// malformed kind is refused with an error naming the field, nothing stored.
pub fn add_memory_with(
    store: &Store<'_>,
    mut input: MemoryAddInput,
    fields: &StructuredFields,
) -> Result<Memory> {
    fields.validate(&input.metadata, None)?;
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let id = format!("mem_{}", uuid::Uuid::new_v4().simple());

    // #260: a no-op unless REMIND_ME_CODE_ROOTS is configured.
    let code_refs = crate::code_refs::detect_code_refs(&input.content);
    crate::code_refs::merge_code_refs(&mut input.metadata, &code_refs);

    // Write boundary: scrub secrets first (so nothing downstream sees them),
    // and resolve attachments before the insert so a bad path fails the add.
    let content = crate::boundary::scrub(&input.content, &mut input.tags, &mut input.metadata);
    let attachments = crate::attachments::resolve(&input.attachments)?;
    crate::attachments::merge_metadata(&mut input.metadata, &attachments);


    // A stated kind drives decay and weight; otherwise the category does, as
    // it always has.
    let kind = fields.memory_type.as_deref().unwrap_or(&input.category);
    let decay_rate = get_decay_rate(kind);
    let type_prior = get_type_prior(kind);
    let source_prior = get_source_prior(&input.source);
    let base_weight = type_prior * source_prior;
    let initial_vitality = calculate_vitality(base_weight, 0, decay_rate, &now_iso, now);

    Memories::new(store).insert(&NewMemory {
        category: input.category.clone(),
        tags: input.tags.clone(),
        source: input.source.clone(),
        metadata: input.metadata.clone(),
        subject: input.subject.clone(),
        predicate: input.predicate.clone(),
        object: input.object.clone(),
        decay_rate,
        vitality: initial_vitality,
        base_weight,
        accessed_at: Some(now_iso.clone()),
        sensitive: input.sensitive,
        memory_type: fields
            .memory_type
            .clone()
            .unwrap_or_else(|| UNCLASSIFIED.to_string()),
        confidence: fields
            .confidence
            .unwrap_or_else(crate::models::default_confidence),
        valid_from: fields.valid_from.clone(),
        valid_until: fields.valid_until.clone(),
        verified_at: fields.verified_at.clone(),
        outcome: fields.outcome.clone(),
        ..crate::context::default_provenance().stamp(NewMemory::new(
            id.clone(),
            content.clone(),
            &now_iso,
        ))
    })?;

    // `MemoryAddInput::entities` was previously parsed and then dropped, so a
    // caller supplying entity mentions got a silent no-op. Same path as
    // `annotate_memories` so both behave identically.
    crate::boundary::index(
        store,
        &id,
        &content,
        &input.entities,
        input.extract,
        &attachments,
        &now_iso,
    )?;

    // Best-effort: no embedder configured, or one that fails mid-request,
    // leaves this memory keyword-searchable only — never a reason to fail
    // the write that already succeeded. `remind_me_reindex` is the backstop
    // for anything that lands here without an embedder available.
    if let Some(embedder) = crate::embedder::available_embedder() {
        let _ = crate::vectors::embed_and_store(store, &*embedder, &id, &content);
    }

    let memory = get_memory_by_id(store, &id)?.ok_or(StoreError::NotFound)?;
    crate::episodes::ensure_session_for(store, &memory)?;

    // Local mutation, so automation hears about it. A record arriving from a
    // peer goes through `sync::upsert_record` instead, which deliberately
    // emits nothing — see `events`' module docs for why echoing sync writes
    // would make two nodes chase each other forever.
    crate::events::emit(crate::events::Event::Created, &memory.id, &memory.category);

    Ok(memory)
}

pub fn get_memory_by_id(store: &Store<'_>, id: &str) -> Result<Option<Memory>> {
    Memories::new(store).get_live(id)
}

/// List memories newest-first, filtered by category, source and/or tags.
///
/// `limit` is clamped to [`LIST_LIMIT_MIN`]..=[`LIST_LIMIT_MAX`]; the clamped
/// value is echoed back in the result so callers can tell what was applied.
pub fn list_memories(store: &Store<'_>, input: &MemoryListInput) -> Result<MemoryListResult> {
    let limit = input.limit.clamp(LIST_LIMIT_MIN, LIST_LIMIT_MAX);
    // Filters apply before `COUNT`, `LIMIT` and `OFFSET`, so all three agree:
    // the reference hit exactly that pagination bug as `DATA-02`.
    let filter = ListFilter {
        include_sensitive: input.include_sensitive,
        category: input.category.clone().filter(|c| !c.is_empty()),
        source: input.source.clone().filter(|s| !s.is_empty()),
        tags: input.tags.clone().unwrap_or_default(),
        scope: input.scope.clone(),
    };
    let (total, memories) = Memories::new(store).list_page(&filter, limit, input.offset)?;

    Ok(MemoryListResult {
        count: memories.len(),
        memories,
        total,
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
pub fn update_memory(store: &Store<'_>, input: &MemoryUpdateInput) -> Result<UpdateOutcome> {
    update_memory_with(store, input, &StructuredFields::default())
}

/// [`update_memory`] with the structured fields a writer may set. Validated
/// against the metadata the memory will hold after this edit and the kind it
/// will have, so `outcome` on a `fact`, or a `decision` with no rationale, is
/// refused before anything is written.
///
/// A new `memory_type` also rewrites `decay_rate`, as `reclassify` does:
/// the type owns the rate.
pub fn update_memory_with(
    store: &Store<'_>,
    input: &MemoryUpdateInput,
    fields: &StructuredFields,
) -> Result<UpdateOutcome> {
    let Some(stored) = get_memory_by_id(store, &input.memory_id)? else {
        return Ok(UpdateOutcome::NotFound);
    };
    let metadata = input.metadata.as_ref().unwrap_or(&stored.metadata);
    fields.validate(metadata, stored.memory_type.as_deref())?;

    // `sensitive` is an `Option<bool>`: `None` leaves the flag alone, so an
    // update that does not mention it cannot silently clear it.
    // `clear_superseded` has one direction only: re-superseding is
    // `remind_me_add`'s job, on detecting a contradiction, never something an
    // update asserts directly.
    let edit = MemoryEdit {
        content: input.content.clone(),
        category: input.category.clone(),
        tags: input.tags.clone(),
        metadata: input.metadata.clone(),
        sensitive: input.sensitive,
        clear_superseded: input.clear_superseded,
        memory_type: fields.memory_type.clone(),
        decay_rate: fields.memory_type.as_deref().map(get_decay_rate),
        confidence: fields.confidence,
        valid_from: fields.valid_from.clone(),
        valid_until: fields.valid_until.clone(),
        verified_at: fields.verified_at.clone(),
        outcome: fields.outcome.clone(),
        ..MemoryEdit::at(Utc::now().to_rfc3339())
    };
    let nothing_to_write = MemoryEdit {
        updated_at: edit.updated_at.clone(),
        ..MemoryEdit::default()
    };
    if edit == nothing_to_write {
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
    crate::history::capture_revision(store, &input.memory_id, &tracked, None)?;

    Memories::new(store).apply_edit(&input.memory_id, &edit)?;

    // Only a content change invalidates the stored embeddings — category,
    // tags and metadata don't change what the text means. Best-effort, same
    // as `add_memory`: an embedding failure here does not undo the update
    // that already committed.
    if let Some(content) = &input.content {
        if let Some(embedder) = crate::embedder::available_embedder() {
            let _ = crate::vectors::embed_and_store(store, &*embedder, &input.memory_id, content);
        }
    }

    let memory = get_memory_by_id(store, &input.memory_id)?.ok_or(StoreError::NotFound)?;
    crate::events::emit(crate::events::Event::Updated, &memory.id, &memory.category);

    Ok(UpdateOutcome::Updated(Box::new(memory)))
}

/// Delete a memory. Returns `false` if no live memory had that id.
///
/// Soft-deletes (tombstones via `deleted_at` + bumps `updated_at`) when sync
/// is configured (`NODE_ID and HUB_URL and SYNC_SECRET`, `#57`), matching the
/// reference exactly: a hard `DELETE` queues no outbox row at all, so it
/// would otherwise silently resurrect on the next pull elsewhere. The tombstone is excluded from every
/// normal read (`deleted_at IS NULL` everywhere this crate reads memories)
/// and, on a node with sync disabled, there is nothing to propagate to, so
/// this is a plain, immediate delete exactly as before.
///
/// The queued payload carries `deleted_at`, so the tombstone travels. The
/// tombstone keeps no text (ADR-0024): its content becomes
/// [`crate::sync::TOMBSTONE_CONTENT`] and its tags and triple go, and its
/// revision history is deleted on either path. It is never purged, since the
/// row is what a stale copy of the memory loses to.
///
/// The FTS row and `memory_tags` go with the write (`db::derived`). Everything else is
/// cleaned up explicitly, because the reference's schema carries **no foreign
/// keys** on `memory_entities`, `memory_feedback` or `memory_associations` —
/// deliberately, since sync can deliver a link before the memory it points at,
/// and a cascade would reject that. This crate previously relied on a cascade
/// it had added itself; regenerating the schema from `remind_me` removed it.
pub fn delete_memory(store: &Store<'_>, memory_id: &str) -> Result<bool> {
    let memories = Memories::new(store);
    // The category is read before either delete path: a hard delete removes
    // the row, so after this point there is nothing left to read it from, and
    // the event would have to guess.
    let Some(category) = memories.live_category(memory_id)? else {
        return Ok(false);
    };

    // A tombstoned memory's embeddings are stale the moment it stops being
    // searchable, same as an incoming sync tombstone's are, so they go on
    // either path.
    crate::vectors::delete_chunks_for_memory(store, memory_id)?;

    let tombstone_at = crate::sync::store_syncs(store)?.then(|| Utc::now().to_rfc3339());
    if !memories.delete_live(memory_id, tombstone_at.as_deref())? {
        return Ok(false);
    }

    // Entities themselves survive — other memories may still mention them.
    crate::db::entities::Entities::new(store).unlink_memory(memory_id)?;
    crate::db::feedback::Feedback::new(store).delete_for(memory_id)?;
    crate::db::related::Related::new(store).unlink_memory(memory_id)?;
    // Its history would keep the text the delete removes (ADR-0024).
    crate::db::history::Revisions::new(store).delete_for(memory_id)?;

    crate::events::emit(crate::events::Event::Deleted, memory_id, &category);

    Ok(true)
}

/// Drop what ADR-0024 says a tombstone must not keep, for the memories
/// deleted before a delete did it: their text, and their revision history.
/// Run at every open; after the first it finds nothing. Returns how many
/// tombstones were emptied and how many revisions went.
pub fn empty_tombstones(store: &Store<'_>) -> Result<(usize, usize)> {
    let emptied = Memories::new(store).empty_tombstones()?;
    let revisions = crate::db::history::Revisions::new(store).delete_of_tombstones()?;
    if emptied > 0 || revisions > 0 {
        eprintln!(
            "rusty-remind-me: dropped the text of {emptied} deleted memories \
             and {revisions} of their revisions (ADR-0024)"
        );
    }
    Ok((emptied, revisions))
}

/// Delete several memories by id in one request.
///
/// Applies the exact same per-memory logic as [`delete_memory`] to each id
/// independently — reused, not reimplemented, so the two paths cannot drift —
/// and one missing id does not fail the rest of the batch.
pub fn bulk_delete(store: &Store<'_>, ids: &[String]) -> Result<BulkDeleteResult> {
    let mut result = BulkDeleteResult::default();
    for id in ids {
        if delete_memory(store, id)? {
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
pub fn bulk_tag(store: &Store<'_>, input: &BulkTagInput) -> Result<BulkTagResult> {
    let memories = Memories::new(store);
    let now = Utc::now().to_rfc3339();
    let mut result = BulkTagResult::default();

    for id in &input.ids {
        let Some(memory) = get_memory_by_id(store, id)? else {
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

        memories.apply_edit(
            id,
            &MemoryEdit {
                tags: Some(new_tags),
                ..MemoryEdit::at(now.as_str())
            },
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
pub fn annotate_memories(store: &Store<'_>, input: &AnnotateInput) -> Result<AnnotateResult> {
    let memories = Memories::new(store);
    let now = Utc::now().to_rfc3339();
    let mut results = Vec::new();
    let mut errors = Vec::new();

    for annotation in &input.annotations {
        if get_memory_by_id(store, &annotation.memory_id)?.is_none() {
            errors.push(AnnotationError {
                memory_id: annotation.memory_id.clone(),
                error: "memory not found".to_string(),
            });
            continue;
        }

        memories.apply_edit(
            &annotation.memory_id,
            &MemoryEdit {
                subject: annotation.subject.clone(),
                predicate: annotation.predicate.clone(),
                object: annotation.object.clone(),
                ..MemoryEdit::at(now.as_str())
            },
        )?;

        let entities_linked = crate::entity::apply_entity_mentions(
            store,
            &annotation.memory_id,
            &annotation.entities,
        )?;

        // After the mentions, never before: the edge is only recorded when both
        // sides of the triple resolve to *known* entities, and the ones this
        // annotation names were created a moment ago.
        crate::entity::maybe_link_entity_relation(
            store,
            annotation.subject.as_deref(),
            annotation.predicate.as_deref(),
            annotation.object.as_deref(),
        )?;

        // Against the triple the memory holds now, which an annotation that
        // set only some of the three completes. Same rule `decompose` applies.
        let superseded_ids = match get_memory_by_id(store, &annotation.memory_id)? {
            Some(m) => crate::entity::supersede_contradicting_facts(
                store,
                &m.id,
                m.subject.as_deref(),
                m.predicate.as_deref(),
                m.object.as_deref(),
            )?,
            None => Vec::new(),
        };

        results.push(AnnotationApplied {
            memory_id: annotation.memory_id.clone(),
            entities_linked,
            superseded_ids,
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
pub fn reclassify_memories(store: &Store<'_>, input: &ReclassifyInput) -> Result<ReclassifyResult> {
    let memories = Memories::new(store);
    let now = Utc::now().to_rfc3339();
    let mut updated = 0;
    let mut not_found = Vec::new();

    for classification in &input.classifications {
        if get_memory_by_id(store, &classification.memory_id)?.is_none() {
            not_found.push(classification.memory_id.clone());
            continue;
        }

        memories.apply_edit(
            &classification.memory_id,
            &MemoryEdit {
                memory_type: Some(classification.memory_type.clone()),
                decay_rate: Some(get_decay_rate(&classification.memory_type)),
                ..MemoryEdit::at(now.as_str())
            },
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
    store: &Store<'_>,
    input: &ReclassifyBatchInput,
) -> Result<ReclassifyBatchResult> {
    let batch_size = input
        .batch_size
        .clamp(RECLASSIFY_BATCH_MIN, RECLASSIFY_BATCH_MAX);

    let (total_unclassified, memories) =
        Memories::new(store).of_type_page(UNCLASSIFIED, batch_size)?;

    Ok(ReclassifyBatchResult {
        memories,
        total_unclassified,
    })
}

/// Search with the configured embedder, if one is resolvable and answering.
///
/// Thin wrapper over [`search_memories_with_embedder`]. Every existing caller
/// keeps its behaviour: `available_embedder` probes the configured backend and
/// yields `None` when none is set or it does not answer, which is the same
/// "keyword-only" case the fused ranker already handles.
pub fn search_memories(
    store: &Store<'_>,
    input: &MemorySearchInput,
) -> Result<Vec<MemorySearchResult>> {
    let configured = crate::embedder::available_embedder();
    search_memories_with_embedder(
        store,
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
    store: &Store<'_>,
    input: &MemorySearchInput,
    embedder: Option<&dyn crate::embedder::Embedder>,
) -> Result<Vec<MemorySearchResult>> {
    Ok(search_memories_budgeted(store, input, embedder)?.results)
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
    store: &Store<'_>,
    input: &MemorySearchInput,
    embedder: Option<&dyn crate::embedder::Embedder>,
) -> Result<crate::retrieval::TrimOutcome> {
    // Reading the deadline here rather than inside means the clock starts
    // before any work — including the FTS query, which is not free on a large
    // store.
    search_memories_deadlined(
        store,
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
    store: &Store<'_>,
    input: &MemorySearchInput,
    embedder: Option<&dyn crate::embedder::Embedder>,
    deadline: crate::retrieval::Deadline,
) -> Result<crate::retrieval::TrimOutcome> {
    let memories_repo = Memories::new(store);
    let mut timing = crate::retrieval::SearchTiming {
        deadline_ms: deadline.limit_ms(),
        ..Default::default()
    };

    let weights = choose_rrf_weights(&input.query, input.strategy);

    // Raw user text is not a valid FTS5 MATCH expression — ordinary punctuation
    // is operator syntax there, so a question like "what's the plan?" was a
    // syntax error rather than a search. No phrases means nothing was
    // searchable.
    let phrases = crate::fts::query_phrases(&input.query);
    if phrases.is_empty() {
        let mut outcome = trim_by_token_budget(Vec::new(), input.token_budget);
        timing.elapsed_ms = deadline.elapsed_ms();
        outcome.timing = timing;
        return Ok(outcome);
    }

    // Dormancy is filtered on *effective* vitality, inside the query and before
    // its limit: the stored column never decays, and thinning a page after the
    // limit under-fills it (the reference's `DI-03`).
    let mut floor = if input.include_dormant {
        None
    } else {
        Some(VITALITY_FLOOR)
    };
    if input.min_vitality > 0.0 {
        floor = Some(floor.map_or(input.min_vitality, |f: f64| f.max(input.min_vitality)));
    }
    let filter = KeywordFilter {
        min_effective_vitality: floor,
        category: input.category.clone(),
        include_sensitive: input.include_sensitive,
        scope: input.scope.clone(),
    };

    // `RrfFusion::Score` mode needs the raw BM25 magnitude alongside each hit.
    let mut keyword_memories = Vec::new();
    let mut keyword_bm25 = std::collections::HashMap::new();
    for (memory, bm25_score) in memories_repo.keyword_hits(&phrases, &filter, input.limit * 2)? {
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
                store,
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
            // Same reason for the project/branch/session/writer scope.
            if !input.scope.is_empty() {
                memories.retain(|m| input.scope.matches_memory(m));
                similarity.retain(|id, _| memories.iter().any(|m| &m.id == id));
            }
            if !input.include_sensitive {
                let hidden = memories_repo.sensitive_ids()?;
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
    let ranked = apply_feedback_adjustment(store, &input.query, ranked)?;

    // Validity and confidence scale the fused score; expired and
    // low-confidence rows can be dropped on request. Before truncation, so
    // a demoted row gives its place to one that is in window.
    let mut ranked = crate::retrieval::apply_validity(
        ranked,
        Utc::now(),
        input.include_expired,
        input.min_confidence,
    );

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
pub fn search_paginated(store: &Store<'_>, input: &SearchPageInput) -> Result<SearchPageResult> {
    let limit = input.limit.clamp(LIST_LIMIT_MIN, LIST_LIMIT_MAX);
    let offset = input.offset;

    // Deliberately narrower than `list_memories`: the reference's `api_search`
    // supports `category` and `tags`, not `source`.
    let mut filter = PageFilter {
        category: input.category.clone().filter(|c| !c.is_empty()),
        tags: input.tags.clone().unwrap_or_default(),
        entity: None,
    };
    if let Some(entity_query) = &input.entity {
        let Some(entity) = crate::entity::resolve_entity(store, entity_query)? else {
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
        filter.entity = Some(EntityScope {
            canonical: crate::entity::normalize_entity_name(&entity.name),
            id: entity.id,
        });
    }

    let phrases = crate::fts::query_phrases(&input.query);
    let (total, memories) = Memories::new(store).keyword_page(&phrases, &filter, limit, offset)?;

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
    store: &Store<'_>,
    input: &MemorySearchInput,
) -> Result<MemorySearchResponse> {
    let configured = crate::embedder::available_embedder();

    // The bootstrap is assembled first because it *spends* from the same
    // budget, and the hits are then searched against what is left. Doing it
    // the other way round — search on the full budget, then prepend — would
    // put the response over budget by exactly the size of the bootstrap,
    // which is the failure #200 spent an issue making visible.
    let bootstrap = if input.bootstrap {
        Some(crate::promotion::bootstrap(store, input.token_budget)?)
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
        store,
        hit_input,
        configured
            .as_ref()
            .map(|e| &**e as &dyn crate::embedder::Embedder),
    )?;
    let memories = outcome.results;
    let ids: Vec<String> = memories.iter().map(|r| r.memory.id.clone()).collect();

    expansion::record_co_retrieval(store, &ids)?;

    // Direct hits only. Expansion results are a discovery aid surfaced by
    // adjacency, not answers to the query, and recording them would inflate
    // the vitality of every neighbour on every expanded search.
    //
    // Ordered after the expansions are built so they read the pre-access state:
    // recording rewrites `vitality` and `accessed_at`, and an expansion should
    // describe the store as the caller found it.
    let response = MemorySearchResponse {
        related_via_entities: if input.expand_entities {
            Some(expansion::expand_via_entities(store, &ids)?)
        } else {
            None
        },
        related_via_neighbors: if input.include_neighbors {
            Some(expansion::expand_via_neighbors(store, &memories)?)
        } else {
            None
        },
        related_via_co_retrieval: if input.expand_co_retrieval {
            Some(expansion::expand_via_co_retrieval(store, &ids)?)
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

    crate::vitality::record_accesses(store, &ids)?;

    Ok(response)
}

/// A page of memories awaiting extraction, newest first.
///
/// The read half of the annotation loop: `remind_me_annotate` writes triples
/// and mentions, and this is what tells a caller which memories still need
/// them. Without it, annotation could only be applied to memories the caller
/// already happened to know about.
pub fn unannotated_batch(
    store: &Store<'_>,
    input: &ExtractBatchInput,
) -> Result<ExtractBatchResult> {
    let batch_size = input.batch_size.clamp(EXTRACT_BATCH_MIN, EXTRACT_BATCH_MAX);
    let (total_unannotated, memories) = Memories::new(store).unannotated_page(batch_size)?;

    Ok(ExtractBatchResult {
        memories,
        total_unannotated,
    })
}
