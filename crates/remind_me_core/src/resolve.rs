//! Outcome tracking: close a decision or action item as done, abandoned,
//! reverted or superseded.
//!
//! A memory that says "we will use X" is dangerous once X was backed out and
//! nothing recorded it. Resolving sets `outcome`, keeps the prior state in
//! history, and demotes what turned out wrong so it stops outranking the
//! decision that replaced it.

use crate::db::feedback::Feedback;
use crate::db::history::Revisions;
use crate::db::memories::{Memories, MemoryEdit};
use crate::db::{Result, Store, StoreError};
use crate::kinds::{KindError, MemoryKind, OUTCOMES};
use crate::models::Memory;
use crate::vitality::{is_dormant, BASE_WEIGHT_MIN};
use chrono::Utc;
use serde_json::{json, Value};

/// The revision reason a resolve leaves in history.
pub const RESOLVE_REASON: &str = "resolve";

/// What a reverted or abandoned memory's `base_weight` is multiplied by.
pub const DEMOTION_FACTOR: f64 = 0.5;

/// Whether this outcome means the memory was wrong or dropped, and so loses
/// weight. `done` and `superseded` leave it alone: the work happened, or a
/// newer memory already outranks it.
pub fn demotes(outcome: &str) -> bool {
    matches!(outcome, "reverted" | "abandoned")
}

/// Resolve memory `memory_id` to `outcome`, with an optional `note`
/// (stored as `metadata.outcome_note`). `None` when no live memory has the id.
///
/// Refused as [`StoreError::Invalid`] for an unknown outcome, or a memory
/// whose kind does not track one (only decisions and action items do).
pub fn resolve_memory(
    store: &Store<'_>,
    memory_id: &str,
    outcome: &str,
    note: Option<&str>,
) -> Result<Option<Memory>> {
    if !OUTCOMES.contains(&outcome) {
        return Err(StoreError::Invalid(format!(
            "`outcome` must be one of {}",
            OUTCOMES.join(", ")
        )));
    }
    let Some(memory) = Memories::new(store).get_live(memory_id)? else {
        return Ok(None);
    };
    let kind = memory
        .memory_type
        .as_deref()
        .and_then(|t| t.parse::<MemoryKind>().ok())
        .unwrap_or(MemoryKind::Unclassified);
    if !kind.tracks_outcome() {
        return Err(KindError::OutcomeNotApplicable(kind.to_string()).into());
    }

    let now = Utc::now().to_rfc3339();
    record_revision(store, memory_id, &now)?;

    let metadata = note
        .filter(|n| !n.trim().is_empty())
        .map(|n| with_note(&memory.metadata, n));
    Memories::new(store).apply_edit(
        memory_id,
        &MemoryEdit {
            outcome: Some(outcome.to_string()),
            metadata,
            ..MemoryEdit::at(now)
        },
    )?;
    if demotes(outcome) {
        demote(store, memory_id)?;
    }
    Memories::new(store).get_live(memory_id)
}

/// Keep the state being replaced in history. Written directly rather than
/// through `history::capture_revision`, which skips an edit that changes no
/// tracked column: a resolve without a note changes none, and is still an
/// event worth a row.
fn record_revision(store: &Store<'_>, memory_id: &str, now: &str) -> Result<()> {
    let revisions = Revisions::new(store);
    let Some(current) = revisions.current(memory_id)? else {
        return Ok(());
    };
    revisions.insert(memory_id, &current, now, Some(RESOLVE_REASON))
}

fn with_note(metadata: &Value, note: &str) -> Value {
    let mut metadata = match metadata {
        Value::Object(_) => metadata.clone(),
        _ => json!({}),
    };
    metadata["outcome_note"] = Value::String(note.to_string());
    metadata
}

/// Halve `base_weight` and recompute the vitality snapshot the way
/// `record_feedback` does, so the memory sinks in ranking.
fn demote(store: &Store<'_>, memory_id: &str) -> Result<()> {
    let feedback = Feedback::new(store);
    let Some(importance) = feedback.importance(memory_id)? else {
        return Ok(());
    };
    let base_weight = (importance.base_weight * DEMOTION_FACTOR).max(BASE_WEIGHT_MIN);
    let vitality = base_weight * ((importance.access_count as f64) + 1.0).sqrt();
    let status = if is_dormant(vitality) {
        "dormant"
    } else {
        "active"
    };
    feedback.set_importance(memory_id, base_weight, vitality, status)
}

/// Check the kinds named by a reclassify batch, against each memory's stored
/// metadata, before any of it is applied: a decision with no rationale is
/// refused with the field named, and nothing is half-written.
///
/// Ids that do not exist are skipped here; `reclassify_memories` reports
/// those as `not_found`.
pub fn validate_reclassify(
    store: &Store<'_>,
    classifications: &[crate::models::MemoryClassification],
) -> Result<()> {
    for c in classifications {
        let kind: MemoryKind = c.memory_type.parse().map_err(StoreError::from)?;
        let Some(memory) = Memories::new(store).get_live(&c.memory_id)? else {
            continue;
        };
        crate::kinds::validate_metadata(kind, &memory.metadata)
            .map_err(|e| StoreError::Invalid(format!("{}: {e}", c.memory_id)))?;
    }
    Ok(())
}

/// Check each decomposed fact's `memory_type` and metadata. A fact with no
/// type is `unclassified` and always valid.
pub fn validate_facts(facts: &[crate::models::AtomicFact]) -> Result<()> {
    for (i, fact) in facts.iter().enumerate() {
        let Some(t) = fact.memory_type.as_deref().filter(|t| !t.is_empty()) else {
            continue;
        };
        let empty = json!({});
        let metadata = fact.metadata.as_ref().unwrap_or(&empty);
        t.parse::<MemoryKind>()
            .and_then(|k| crate::kinds::validate_metadata(k, metadata))
            .map_err(|e| StoreError::Invalid(format!("facts[{i}]: {e}")))?;
    }
    Ok(())
}
