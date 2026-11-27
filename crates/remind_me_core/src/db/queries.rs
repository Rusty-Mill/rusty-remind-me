use super::{Result, Store, StoreError};
use crate::db::memories::{
    is_raw_category, EntityMembers, EntityScope, KeywordFilter, ListFilter, Memories, MemoryEdit,
    Narrowing, NewMemory, PageFilter,
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
    apply_feedback_adjustment, calculate_vitality, get_decay_rate, get_source_prior, get_type_prior,
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
    input: MemoryAddInput,
    fields: &StructuredFields,
) -> Result<Memory> {
    add_memory_reviewing(store, input, fields, Review::Schedule)
}

/// Whether a new decision gets a review date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Review {
    /// The configured interval from now (ADR-0031 §1).
    Schedule,
    /// None: a decision made before it was written down, such as one
    /// imported from a repo log (pack D74).
    Unscheduled,
}

/// [`add_memory_with`], scheduling a decision's review only as `review`
/// says.
pub(crate) fn add_memory_reviewing(
    store: &Store<'_>,
    input: MemoryAddInput,
    fields: &StructuredFields,
    review: Review,
) -> Result<Memory> {
    let supersedes = input.supersedes.clone();
    check_dead_end_source(store, fields.memory_type.as_deref(), &input.metadata)?;
    let add = PreparedAdd::new(input, fields, review)?;
    // With `supersedes`, one transaction: the insert, its index and every
    // retirement land together or not at all, and each target is checked
    // inside it, so no other writer can change it between check and write.
    // Without, the add stays the plain write it always was.
    if supersedes.is_empty() {
        add.write(store, &[])?;
    } else {
        store.transaction(|tx| add.write(tx, &supersedes))?;
    }
    add.finish(store)
}

/// A dead end's `metadata.from` names a live memory (card #1484, D123):
/// the decision or action item it came from. Other kinds are not checked.
fn check_dead_end_source(
    store: &Store<'_>,
    kind: Option<&str>,
    metadata: &serde_json::Value,
) -> Result<()> {
    if kind != Some(crate::kinds::MemoryKind::DeadEnd.as_str()) {
        return Ok(());
    }
    let Some(from) = metadata.get("from").and_then(serde_json::Value::as_str) else {
        return Ok(());
    };
    if Memories::new(store).get_live(from.trim())?.is_some() {
        return Ok(());
    }
    Err(StoreError::Invalid(format!(
        "dead_end metadata.from {from:?} is not a live memory id"
    )))
}

/// An add, validated and scrubbed, split into its write and what follows
/// it, so a caller can make the write one step of a larger transaction and
/// run the rest once that commits.
pub(crate) struct PreparedAdd {
    id: String,
    content: String,
    row: NewMemory,
    input: MemoryAddInput,
    attachments: Vec<crate::attachments::ResolvedAttachment>,
    now_iso: String,
}

impl PreparedAdd {
    /// Validate and scrub `input`; nothing is written. Its `supersedes` is
    /// ignored: [`PreparedAdd::write`] takes the targets.
    pub(crate) fn new(
        mut input: MemoryAddInput,
        fields: &StructuredFields,
        review: Review,
    ) -> Result<Self> {
        // The evidence count is the system's (#354): a client cannot seed one.
        input.metadata =
            crate::consolidation::keep_evidence(std::mem::take(&mut input.metadata), None);
        fields.validate(&input.metadata, None)?;
        crate::kinds::validate_occurred_at(&input.metadata)?;
        crate::project::refuse_reserved_predicate(input.predicate.as_deref())?;
        crate::topic::refuse_topic_entities(&input.entities)?;
        let project = crate::context::explicit_project(input.project.as_deref())?;
        let scheduled = match review {
            Review::Schedule => {
                crate::review::schedule_if_decision(fields.memory_type.as_deref(), &input.metadata)
            }
            Review::Unscheduled => None,
        };
        if let Some(scheduled) = scheduled {
            input.metadata = scheduled;
        }
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

        let mut provenance = crate::context::default_provenance();
        if project.is_some() {
            provenance.context.project = project;
        }
        let row = NewMemory {
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
            ..provenance.stamp(NewMemory::new(id.clone(), content.clone(), &now_iso))
        };
        if row.category == crate::models::PERSONA_CATEGORY {
            crate::kinds::validate_new_persona(&row.metadata, &row.written_by)?;
        }
        Ok(Self {
            id,
            content,
            row,
            input,
            attachments,
            now_iso,
        })
    }

    /// The new memory's id.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Insert the memory and its index, and retire `supersedes`, each
    /// checked to be a live memory. Joins the caller's transaction.
    pub(crate) fn write(&self, tx: &Store<'_>, supersedes: &[String]) -> Result<()> {
        Memories::new(tx).insert(&self.row)?;
        // `MemoryAddInput::entities` was previously parsed and then dropped, so a
        // caller supplying entity mentions got a silent no-op. Same path as
        // `annotate_memories` so both behave identically.
        crate::boundary::index(
            tx,
            &self.id,
            &self.content,
            &self.input.entities,
            self.input.extract,
            &self.attachments,
            &self.now_iso,
        )?;
        // Stamped, so the change reaches the sync outbox.
        for old in supersedes {
            retire(tx, old, &self.id, &self.now_iso)?;
        }
        Ok(())
    }

    /// What follows a committed write: embedding, topic, session, event.
    /// Run outside any transaction.
    pub(crate) fn finish(&self, store: &Store<'_>) -> Result<Memory> {
        // Best-effort: no embedder configured, or one that fails mid-request,
        // leaves this memory keyword-searchable only — never a reason to fail
        // the write that already succeeded. `remind_me_reindex` is the backstop
        // for anything that lands here without an embedder available.
        if let Some(embedder) = crate::embedder::available_embedder() {
            let _ = crate::vectors::embed_and_store(store, &*embedder, &self.id, &self.content);
        }
        // Best-effort like the embedding above: a write never fails for want of
        // a topic. Reads the saved centroids only; a store without topics pays a
        // file-existence check.
        crate::topic_assign::assign_on_write(store, &self.id);

        let memory = get_memory_by_id(store, &self.id)?.ok_or(StoreError::NotFound)?;
        crate::episodes::ensure_session_for(store, &memory)?;

        // Local mutation, so automation hears about it.
        crate::events::emit(crate::events::Event::Created, &memory.id, &memory.category);

        Ok(memory)
    }
}

/// Mark live memory `old` superseded by `new`, refusing one that is missing,
/// deleted, already superseded or a project claim (claims are append-only).
fn retire(store: &Store<'_>, old: &str, new: &str, now_iso: &str) -> Result<()> {
    let target = get_memory_by_id(store, old)?.filter(|m| m.superseded_by.is_none());
    let Some(target) = target else {
        return Err(StoreError::Invalid(format!(
            "supersedes: {old:?} is not a live memory (missing, deleted or already superseded)"
        )));
    };
    crate::dialog::refuse_protected_edit(&target)?;
    Memories::new(store).set_superseded_by(old, new, Some(now_iso))
}

pub fn get_memory_by_id(store: &Store<'_>, id: &str) -> Result<Option<Memory>> {
    Memories::new(store).get_live(id)
}

/// List memories newest-first, filtered by category, source and/or tags.
///
/// A `limit` below [`LIST_LIMIT_MIN`] is refused as [`StoreError::Invalid`]:
/// it used to clamp to 1, so a caller asking for nothing silently got one row
/// (#223). A `limit` above [`LIST_LIMIT_MAX`] is clamped, and the applied
/// value is echoed back in the result so callers can tell.
pub fn list_memories(store: &Store<'_>, input: &MemoryListInput) -> Result<MemoryListResult> {
    if input.limit < LIST_LIMIT_MIN {
        return Err(StoreError::Invalid(format!(
            "limit must be between {LIST_LIMIT_MIN} and {LIST_LIMIT_MAX}, got {}",
            input.limit
        )));
    }
    let limit = input.limit.min(LIST_LIMIT_MAX);
    // Filters apply before `COUNT`, `LIMIT` and `OFFSET`, so all three agree:
    // the reference hit exactly that pagination bug as `DATA-02`.
    let filter = ListFilter {
        include_sensitive: input.include_sensitive,
        category: input.category.clone().filter(|c| !c.is_empty()),
        source: input.source.clone().filter(|s| !s.is_empty()),
        tags: input.tags.clone().unwrap_or_default(),
        scope: input.scope.resolve(store)?,
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

/// Persona rules on an edit (#1555, pack D102): a persona row's metadata
/// is checked; a row moved into `persona` must meet what a new hand-written
/// line must, judged by who wrote it.
fn check_persona_edit(
    stored: &Memory,
    new_category: Option<&str>,
    metadata: &serde_json::Value,
) -> Result<()> {
    let persona = crate::models::PERSONA_CATEGORY;
    let target = new_category
        .filter(|c| !c.is_empty())
        .unwrap_or(&stored.category);
    if target != persona {
        return Ok(());
    }
    if stored.category == persona {
        crate::kinds::validate_persona(metadata)?;
    } else {
        crate::kinds::validate_new_persona(metadata, &stored.written_by)?;
    }
    Ok(())
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
    crate::dialog::refuse_protected_edit(&stored)?;
    let project = crate::context::explicit_project(input.project.as_deref())?;
    // Replacing metadata keeps the system-owned evidence count (#354), so an
    // edit can neither clobber it nor reset it under an already boosted
    // `base_weight`.
    let client_metadata = input
        .metadata
        .clone()
        .map(|m| crate::consolidation::keep_evidence(m, Some(&stored.metadata)));
    let metadata = client_metadata.as_ref().unwrap_or(&stored.metadata);
    fields.validate(metadata, stored.memory_type.as_deref())?;
    if client_metadata.is_some() || fields.memory_type.is_some() {
        let kind = fields
            .memory_type
            .as_deref()
            .or(stored.memory_type.as_deref());
        check_dead_end_source(store, kind, metadata)?;
    }
    // Only metadata this edit supplies: a stored value is never revalidated,
    // so an edit that leaves metadata alone cannot fail on it (pack D86).
    if let Some(supplied) = &client_metadata {
        crate::kinds::validate_occurred_at(supplied)?;
    }
    check_persona_edit(&stored, input.category.as_deref(), metadata)?;
    // Becoming a decision schedules its follow-up; staying one does not.
    let scheduled = if stored.memory_type.as_deref() == Some("decision") {
        None
    } else {
        crate::review::schedule_if_decision(fields.memory_type.as_deref(), metadata)
    };

    // `sensitive` is an `Option<bool>`: `None` leaves the flag alone, so an
    // update that does not mention it cannot silently clear it.
    // `clear_superseded` has one direction only: re-superseding is
    // `remind_me_add`'s job, on detecting a contradiction, never something an
    // update asserts directly.
    let edit = MemoryEdit {
        content: input.content.clone(),
        category: input.category.clone(),
        tags: input.tags.clone(),
        metadata: scheduled.or_else(|| client_metadata.clone()),
        sensitive: input.sensitive,
        clear_superseded: input.clear_superseded,
        memory_type: fields.memory_type.clone(),
        decay_rate: fields.memory_type.as_deref().map(get_decay_rate),
        confidence: fields.confidence,
        valid_from: fields.valid_from.clone(),
        valid_until: fields.valid_until.clone(),
        verified_at: fields.verified_at.clone(),
        outcome: fields.outcome.clone(),
        project,
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
        metadata_json: client_metadata
            .as_ref()
            .map(|m| serde_json::to_string(m).unwrap_or_else(|_| "{}".to_string())),
        sensitive: input.sensitive,
        project: edit.project.clone().map(Some),
    };
    // The revision and the edit land together or not at all (Q21): a
    // revision without its edit would hold the current value as history.
    store.atomically(|store| -> Result<()> {
        crate::history::capture_revision(store, &input.memory_id, &tracked, None)?;
        Memories::new(store).apply_edit(&input.memory_id, &edit)
    })?;

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
/// Soft-deletes (tombstones via `deleted_at` + bumps `updated_at`) in a store
/// that synced ([`crate::sync::store_syncs`]), so its outbox and its copies
/// agree; the tombstone is excluded from every normal read. A store that
/// never synced has nothing to propagate to, so this is a plain, immediate
/// delete.
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
    refuse_protected(store, &[memory_id.to_string()])?;

    // Every part of the delete lands, or none does (Q21): a crash between
    // them must not leave a deleted memory's revisions holding its text.
    let deleted = store.atomically(|store| -> Result<bool> {
        // A tombstoned memory's embeddings are stale the moment it stops
        // being searchable, same as an incoming sync tombstone's are, so
        // they go on either path.
        crate::vectors::delete_chunks_for_memory(store, memory_id)?;

        let tombstone_at = crate::sync::store_syncs(store)?.then(|| Utc::now().to_rfc3339());
        if !Memories::new(store).delete_live(memory_id, tombstone_at.as_deref())? {
            return Ok(false);
        }

        // Entities themselves survive — other memories may still mention them.
        crate::db::entities::Entities::new(store).unlink_memory(memory_id)?;
        crate::db::feedback::Feedback::new(store).delete_for(memory_id)?;
        crate::db::related::Related::new(store).unlink_memory(memory_id)?;
        // Its history would keep the text the delete removes (ADR-0024).
        crate::db::history::Revisions::new(store).delete_for(memory_id)?;
        Ok(true)
    })?;
    if !deleted {
        return Ok(false);
    }

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
    // Checked before any delete, so a claim or dialog in the batch refuses all of it.
    refuse_protected(store, ids)?;
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

/// Refuse when any of `ids` is append-only: a project claim (ADR-0027) or a
/// dialog row ([`crate::dialog`]). Missing ids are left to the caller.
fn refuse_protected(store: &Store<'_>, ids: &[String]) -> Result<()> {
    let memories = Memories::new(store);
    for id in ids {
        if let Some(memory) = memories.get_live(id)? {
            crate::dialog::refuse_protected_edit(&memory)?;
        }
    }
    Ok(())
}

/// Add, remove, or replace tags on several memories in one request.
///
/// A missing id is recorded in `not_found` and the rest of the batch still
/// applies, matching [`bulk_delete`]'s per-item error handling.
pub fn bulk_tag(store: &Store<'_>, input: &BulkTagInput) -> Result<BulkTagResult> {
    refuse_protected(store, &input.ids)?;
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
        let Some(stored) = get_memory_by_id(store, &annotation.memory_id)? else {
            errors.push(AnnotationError {
                memory_id: annotation.memory_id.clone(),
                error: "memory not found".to_string(),
            });
            continue;
        };
        let guarded = crate::project::refuse_reserved_predicate(annotation.predicate.as_deref())
            .and_then(|()| crate::dialog::refuse_protected_edit(&stored))
            .and_then(|()| crate::topic::refuse_topic_entities(&annotation.entities));
        if let Err(refused) = guarded {
            errors.push(AnnotationError {
                memory_id: annotation.memory_id.clone(),
                error: refused.to_string(),
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
    let mut refused = Vec::new();

    for classification in &input.classifications {
        let Some(stored) = get_memory_by_id(store, &classification.memory_id)? else {
            not_found.push(classification.memory_id.clone());
            continue;
        };
        if crate::dialog::is_dialog(&stored) {
            refused.push(classification.memory_id.clone());
            continue;
        }
        // Becoming a decision schedules its follow-up; staying one does not.
        let metadata = if stored.memory_type.as_deref() == Some("decision") {
            None
        } else {
            crate::review::schedule_if_decision(
                Some(classification.memory_type.as_str()),
                &stored.metadata,
            )
        };

        memories.apply_edit(
            &classification.memory_id,
            &MemoryEdit {
                metadata,
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
        refused,
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
    // syntax error rather than a search. No phrases leaves nothing to rank —
    // handled below, once the filter exists.
    let phrases = crate::fts::query_phrases(&input.query);
    // An entity the store has never seen matches nothing: an empty result,
    // not an error, as `search_paginated` answers it.
    let scope = input.scope.resolve(store)?;
    let Some(narrowing) = resolve_narrowing(store, input)? else {
        let mut outcome = trim_by_token_budget(Vec::new(), input.token_budget, input.detail);
        timing.elapsed_ms = deadline.elapsed_ms();
        outcome.timing = timing;
        return Ok(outcome);
    };

    // Dormancy is filtered on *effective* vitality, inside the query and before
    // its limit: the stored column never decays, and thinning a page after the
    // limit under-fills it (the reference's `DI-03`).
    // Decisions and preferences are exempt from the default floor, not from a
    // caller's explicit `min_vitality` (Millwright R1).
    let filter = KeywordFilter {
        hide_dormant: !input.include_dormant,
        min_effective_vitality: (input.min_vitality > 0.0).then_some(input.min_vitality),
        category: input.category.clone(),
        include_sensitive: input.include_sensitive,
        scope: scope.clone(),
        exclude_raw: !input.ranks_raw(),
        narrowing: narrowing.clone(),
        window: input.window,
    };

    // Nothing to rank. A bounded window still asks a question — "what
    // happened in the last two days" — so answer it as a newest-first listing;
    // with no window either, nothing was searchable.
    // `limit` applies after `apply_validity`, so a dropped or demoted row
    // gives its place to an eligible one, as in the ranked path.
    if phrases.is_empty() {
        let mut dormant_excluded = 0;
        let listed = if input.window.is_unbounded() {
            Vec::new()
        } else {
            // With dormant rows admitted, so the ones the floor hides can be
            // counted (#230); then held to the floor like any listing.
            let now = Utc::now();
            let (hits, dormant): (Vec<Memory>, Vec<Memory>) = memories_repo
                .window_hits(&KeywordFilter {
                    hide_dormant: false,
                    ..filter.clone()
                })?
                .into_iter()
                .partition(|m| filter.vital_enough(m, now));
            dormant_excluded = dormant
                .iter()
                .filter(|m| dormant_only(&filter, input, m, now))
                .count();
            let mut listed = crate::retrieval::apply_validity(
                hits.into_iter().map(listed_result).collect(),
                Utc::now(),
                input.include_expired,
                input.min_confidence,
            );
            listed.truncate(input.limit);
            listed
        };
        if listed.is_empty() && filter.exclude_raw && !input.window.is_unbounded() {
            return raw_fallback(store, input, embedder, deadline, timing);
        }
        let mut outcome = trim_by_token_budget(listed, input.token_budget, input.detail);
        timing.elapsed_ms = deadline.elapsed_ms();
        outcome.timing = timing;
        outcome.dormant_excluded = dormant_excluded;
        return Ok(outcome);
    }

    // `RrfFusion::Score` mode needs the raw BM25 magnitude alongside each hit.
    let mut keyword_memories = Vec::new();
    let mut keyword_bm25 = std::collections::HashMap::new();
    for (memory, bm25_score) in memories_repo.keyword_hits(&phrases, &filter, input.limit * 2)? {
        keyword_bm25.insert(memory.id.clone(), bm25_score);
        keyword_memories.push(memory);
    }
    // Dormant matches the floor kept out, from both halves, so a reader can
    // tell "nothing matched" from "what matched has gone dormant" (#230).
    let dormant_ids = std::cell::RefCell::new(dormant_keyword_matches(
        &memories_repo,
        &phrases,
        &filter,
        input,
        input.limit * 2,
    )?);

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
    let semantic_embedder = match embedder {
        Some(_) if deadline.expired() => {
            timing.skipped.push("semantic".to_string());
            None
        }
        other => other,
    };

    let (semantic_memories, semantic_similarity) = match semantic_embedder {
        Some(semantic) => {
            let extra_texts = crate::query_expansion::expand_query(&input.query);
            // Filtering the keyword half alone would not be enough: RRF fuses
            // two independent result sets, so a sensitive, out-of-scope,
            // out-of-window or raw memory excluded there could still arrive
            // through the semantic half. Admitted inside the scan, before its limit,
            // so the filters never starve the semantic list. That includes the
            // vitality floor: a dormant row kept out of the keyword half must
            // not return through vectors (#564).
            let now = Utc::now();
            let hidden = if input.include_sensitive {
                std::collections::HashSet::new()
            } else {
                memories_repo.sensitive_ids()?
            };
            let admit = |m: &Memory| {
                let eligible = !hidden.contains(&m.id)
                    && scope.matches_memory(m)
                    && input.window.contains_memory(m)
                    && narrowing.admits(m)
                    // Raw rows go before the best `limit * 2` are chosen, or
                    // stronger raw hits would take every slot (ADR-0029).
                    && !(filter.exclude_raw && is_raw_category(&m.category));
                if !eligible {
                    return false;
                }
                if filter.vital_enough(m, now) {
                    return true;
                }
                if dormant_only(&filter, input, m, now) {
                    dormant_ids.borrow_mut().insert(m.id.clone());
                }
                false
            };
            // A failure here still degrades to keyword-only, as before, but
            // is reported as a failed stage rather than passing for a clean
            // search that found nothing semantically.
            let scored = match crate::vectors::semantic_search_admitted(
                store,
                semantic,
                &input.query,
                &extra_texts,
                input.limit * 2,
                input.category.as_deref(),
                &admit,
            ) {
                Ok(scored) => scored,
                // The error itself is not logged: an embedding error can
                // carry the provider's raw response body.
                Err(_) => {
                    eprintln!("rusty-remind-me: a search's semantic stage failed");
                    timing.failed.push("semantic".to_string());
                    Vec::new()
                }
            };
            let mut memories = Vec::with_capacity(scored.len());
            let mut similarity = std::collections::HashMap::with_capacity(scored.len());
            for (memory, sim) in scored {
                similarity.insert(memory.id.clone(), sim as f64);
                memories.push(memory);
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

    if ranked.is_empty() && filter.exclude_raw {
        return raw_fallback(store, input, embedder, deadline, timing);
    }

    let mut outcome = trim_by_token_budget(ranked, input.token_budget, input.detail);
    timing.elapsed_ms = deadline.elapsed_ms();
    outcome.timing = timing;
    outcome.dormant_excluded = dormant_ids.into_inner().len();
    Ok(outcome)
}

/// Whether the dormancy floor alone keeps `memory` out: it is dormant, yet
/// clears the caller's explicit `min_effective_vitality` and the validity
/// bars `apply_validity` holds every hit to (expiry, `min_confidence`). Such
/// a row is one `include_dormant` would return, so only it is counted.
fn dormant_only(
    filter: &KeywordFilter,
    input: &MemorySearchInput,
    memory: &Memory,
    now: chrono::DateTime<Utc>,
) -> bool {
    let floor_only = KeywordFilter {
        hide_dormant: false,
        ..filter.clone()
    };
    filter.hide_dormant
        && !filter.vital_enough(memory, now)
        && floor_only.vital_enough(memory, now)
        && crate::retrieval::passes_validity(
            memory,
            now,
            input.include_expired,
            input.min_confidence,
        )
}

/// The ids of keyword matches that only the dormancy floor left out
/// ([`dormant_only`]), among the first `limit` matches with dormant ones
/// admitted. Empty when dormant memories were not hidden. One extra bounded
/// full-text query.
fn dormant_keyword_matches(
    memories: &Memories<'_>,
    phrases: &[String],
    filter: &KeywordFilter,
    input: &MemorySearchInput,
    limit: usize,
) -> Result<std::collections::HashSet<String>> {
    if !filter.hide_dormant || phrases.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let with_dormant = KeywordFilter {
        hide_dormant: false,
        ..filter.clone()
    };
    let now = Utc::now();
    Ok(memories
        .keyword_hits(phrases, &with_dormant, limit)?
        .into_iter()
        .map(|(memory, _)| memory)
        .filter(|m| dormant_only(filter, input, m, now))
        .map(|m| m.id)
        .collect())
}

/// Nothing refined matched: a session not yet decomposed is still findable
/// through its dialog, so `input` runs again with raw rows admitted and the
/// result is marked (ADR-0029). Same embedder and deadline, so the second
/// pass skips what the first skipped; both passes' skipped and failed stages
/// are kept.
fn raw_fallback(
    store: &Store<'_>,
    input: &MemorySearchInput,
    embedder: Option<&dyn crate::embedder::Embedder>,
    deadline: crate::retrieval::Deadline,
    first: crate::retrieval::SearchTiming,
) -> Result<crate::retrieval::TrimOutcome> {
    let raw = MemorySearchInput {
        include_raw: true,
        ..input.clone()
    };
    let mut outcome = search_memories_deadlined(store, &raw, embedder, deadline)?;
    outcome.raw_fallback = !outcome.results.is_empty();
    for stage in first.skipped {
        if !outcome.timing.skipped.contains(&stage) {
            outcome.timing.skipped.push(stage);
        }
    }
    for stage in first.failed {
        if !outcome.timing.failed.contains(&stage) {
            outcome.timing.failed.push(stage);
        }
    }
    Ok(outcome)
}

/// `input`'s tag, entity and repo filters, resolved against the store.
/// `None` when the entity names nothing this store knows, so no memory can
/// match. Blank values are unset.
fn resolve_narrowing(store: &Store<'_>, input: &MemorySearchInput) -> Result<Option<Narrowing>> {
    fn clean(v: &Option<String>) -> Option<&str> {
        v.as_deref().map(str::trim).filter(|v| !v.is_empty())
    }
    let entity = match clean(&input.filters.entity) {
        None => None,
        Some(name) => {
            let Some(entity) = crate::entity::resolve_entity(store, name)? else {
                return Ok(None);
            };
            Some(EntityMembers {
                canonical: crate::entity::normalize_entity_name(&entity.name),
                linked: Memories::new(store).linked_ids(&entity.id)?,
            })
        }
    };
    let topic = match clean(&input.filters.topic) {
        None => None,
        Some(name) => {
            let Ok(name) = crate::topic::TopicName::parse(name) else {
                return Ok(None);
            };
            let tree = crate::topic::tree(store)?;
            let subtree = tree.subtree(&name);
            if subtree.is_empty() {
                return Ok(None);
            }
            let memories = Memories::new(store);
            let mut members = std::collections::HashSet::new();
            for t in subtree {
                members.extend(memories.linked_ids(&t.entity_id)?);
            }
            Some(members)
        }
    };
    let tags = input
        .tags
        .iter()
        .flatten()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    Ok(Some(Narrowing {
        tags,
        tag_match: input.filters.tag_mode,
        entity,
        topic,
        repo: clean(&input.filters.repo).and_then(crate::context::normalize_remote),
        projectless: false,
    }))
}

/// A window listing's row. There is no relevance to score, so every row gets
/// the same 1.0: the newest-first order survives `apply_validity`'s stable
/// sort, and its validity and confidence factors still push expired and
/// low-confidence rows down, as they do in a ranked search.
fn listed_result(memory: Memory) -> MemorySearchResult {
    MemorySearchResult {
        memory,
        score: 1.0,
        fts_score: None,
        vec_score: None,
        recency_score: None,
        vitality_score: None,
        idf_score: None,
        feedback_adjustment: None,
        rerank_score: None,
        search_method: None,
    }
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
    // A date phrase in the query becomes the window here, at the tool's
    // boundary, and nowhere deeper: saved searches, the brief and export call
    // the ranker directly and keep searching their text as written.
    let inferred = if crate::date_inference::enabled() {
        crate::date_inference::infer(input, Utc::now(), crate::date_phrase::configured_zone)
            .map_err(StoreError::Invalid)?
    } else {
        None
    };
    let narrowed;
    let (input, inferred_window) = match inferred {
        Some((rewritten, applied)) => {
            narrowed = rewritten;
            (&narrowed, Some(applied))
        }
        None => (input, None),
    };

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
    // Dead-end warnings spend from the budget next, like the bootstrap, and
    // reserve at most a quarter of it (card #1484, D117).
    let found = search_warnings(store, input)?;
    let spent = bootstrap.as_ref().map_or(0, |b| b.tokens_used) + found.tokens;
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

    let embedder = configured
        .as_ref()
        .map(|e| &**e as &dyn crate::embedder::Embedder);
    let outcome = hits_without_warned(store, hit_input, embedder, &found.warnings)?;
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
        raw_fallback: outcome.raw_fallback,
        dormant_excluded: outcome.dormant_excluded,
        bootstrap,
        inferred_window,
        warnings: found.warnings,
        warnings_omitted: found.omitted,
        memories,
        total_candidates: outcome.total_candidates,
        returned: outcome.returned,
        trimmed: outcome.trimmed,
        trimmed_ids: outcome.trimmed_ids,
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

/// The ranked hits less any dead end already shown as a warning (card
/// #1484). The warned ids are dropped before `limit` and the token budget
/// apply, so the remaining matches fill the page and the envelope counts only
/// what is returned.
fn hits_without_warned(
    store: &Store<'_>,
    input: &MemorySearchInput,
    embedder: Option<&dyn crate::embedder::Embedder>,
    warnings: &[crate::dead_ends::Warning],
) -> Result<crate::retrieval::TrimOutcome> {
    if warnings.is_empty() {
        return search_memories_budgeted(store, input, embedder);
    }
    let warned: std::collections::HashSet<&str> = warnings.iter().map(|w| w.id.as_str()).collect();
    let wide = MemorySearchInput {
        limit: input.limit + warned.len(),
        token_budget: 0,
        ..input.clone()
    };
    let raw = search_memories_budgeted(store, &wide, embedder)?;
    let mut kept: Vec<_> = raw
        .results
        .into_iter()
        .filter(|r| !warned.contains(r.memory.id.as_str()))
        .collect();
    kept.truncate(input.limit);
    let mut outcome = trim_by_token_budget(kept, input.token_budget, input.detail);
    outcome.timing = raw.timing;
    outcome.raw_fallback = raw.raw_fallback;
    outcome.dormant_excluded = raw.dormant_excluded;
    Ok(outcome)
}

/// A search's dead-end warnings and the tokens they spend.
struct SearchWarnings {
    warnings: Vec<crate::dead_ends::Warning>,
    omitted: usize,
    tokens: usize,
}

/// The dead ends a search's query repeats (card #1484, D116-D117): at most
/// [`crate::dead_ends::SEARCH_LIMIT`], within a quarter of the token budget
/// (none dropped when it is unlimited). A listing (empty query) has none.
fn search_warnings(store: &Store<'_>, input: &MemorySearchInput) -> Result<SearchWarnings> {
    let query = input.query.trim();
    if query.is_empty() {
        return Ok(SearchWarnings {
            warnings: Vec::new(),
            omitted: 0,
            tokens: 0,
        });
    }
    let request = crate::dead_ends::Request {
        query: Some(query),
        project: input.scope.project.as_deref(),
        include_sensitive: input.include_sensitive,
    };
    let found = crate::dead_ends::find(
        store,
        &request,
        crate::dead_ends::SEARCH_LIMIT,
        Utc::now(),
        &|_| true,
    )?;
    let reserve = input.token_budget / 4;
    let mut kept = Vec::new();
    let mut tokens = 0;
    let mut omitted = found.omitted;
    for warning in found.warnings {
        let cost = crate::retrieval::estimated_content_tokens(
            &serde_json::to_string(&warning).unwrap_or_default(),
        );
        if input.token_budget > 0 && tokens + cost > reserve {
            omitted += 1;
            continue;
        }
        tokens += cost;
        kept.push(warning);
    }
    Ok(SearchWarnings {
        warnings: kept,
        omitted,
        tokens,
    })
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
