//! Outcome tracking: close a decision or action item as done, abandoned,
//! reverted, superseded or carded (moved to a board card).
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
/// weight. `done`, `superseded` and `carded` leave it alone: the work happened,
/// a newer memory already outranks it, or it moved to a board card and is
/// still live there.
pub fn demotes(outcome: &str) -> bool {
    matches!(outcome, "reverted" | "abandoned")
}

/// What resolve's reply suggests after an outcome that may mean an approach
/// failed (card #1484, D123): record a dead end that links back here. Never
/// created automatically, since "abandoned" often means deprioritised.
/// `None` for the other outcomes.
pub fn dead_end_hint(outcome: &str, memory_id: &str) -> Option<String> {
    demotes(outcome).then(|| {
        format!(
            "If this failed rather than being dropped, record why so it is not tried again: \
             remind_me_add with memory_type dead_end, metadata {{tried, why_failed, cost?, \
             from: \"{memory_id}\"}}."
        )
    })
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
    let metadata = note
        .filter(|n| !n.trim().is_empty())
        .map(|n| with_note(&memory.metadata, n));
    let edit = MemoryEdit {
        outcome: Some(outcome.to_string()),
        metadata,
        ..MemoryEdit::at(now.as_str())
    };
    write_outcome(
        store,
        memory_id,
        &now,
        RESOLVE_REASON,
        &edit,
        demotes(outcome),
    )?;
    Memories::new(store).get_live(memory_id)
}

/// Record the revision `reason` leaves, apply `edit`, and [`demote`] the
/// memory when `demote_it`, all in one transaction (joining the caller's,
/// if one is open): an edit that fails leaves no revision of a change that
/// never happened (#1595). Shared with [`crate::review`].
pub(crate) fn write_outcome(
    store: &Store<'_>,
    memory_id: &str,
    now: &str,
    reason: &str,
    edit: &MemoryEdit,
    demote_it: bool,
) -> Result<()> {
    store.atomically(|store| {
        record_revision(store, memory_id, now, reason)?;
        #[cfg(test)]
        if FAIL_EDIT.with(|fail| fail.replace(false)) {
            return Err(StoreError::Engine("injected outcome edit failure".into()));
        }
        Memories::new(store).apply_edit(memory_id, edit)?;
        if demote_it {
            demote(store, memory_id)?;
        }
        Ok(())
    })
}

#[cfg(test)]
thread_local! {
    /// Fail the next [`write_outcome`] on this thread after its revision is
    /// written and before its edit, to prove neither lands (#1595).
    pub(crate) static FAIL_EDIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Keep the state being replaced in history. Written directly rather than
/// through `history::capture_revision`, which skips an edit that changes no
/// tracked column: a resolve without a note changes none, and is still an
/// event worth a row. Shared with [`crate::review`], whose reviews are the
/// same kind of event.
pub(crate) fn record_revision(
    store: &Store<'_>,
    memory_id: &str,
    now: &str,
    reason: &str,
) -> Result<()> {
    let revisions = Revisions::new(store);
    let Some(current) = revisions.current(memory_id)? else {
        return Ok(());
    };
    revisions.insert(memory_id, &current, now, Some(reason))
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
pub(crate) fn demote(store: &Store<'_>, memory_id: &str) -> Result<()> {
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
/// type is `unclassified`; only its `basis` is checked. Every fact's
/// `occurred_at` is checked whatever its type.
pub fn validate_facts(facts: &[crate::models::AtomicFact]) -> Result<()> {
    for (i, fact) in facts.iter().enumerate() {
        let empty = json!({});
        let metadata = fact.metadata.as_ref().unwrap_or(&empty);
        let checked = match fact.memory_type.as_deref().filter(|t| !t.is_empty()) {
            Some(t) => t
                .parse::<MemoryKind>()
                .and_then(|k| crate::kinds::validate_metadata(k, metadata)),
            None => crate::kinds::validate_basis(metadata),
        };
        let checked = checked.and_then(|()| crate::kinds::validate_occurred_at(metadata));
        checked.map_err(|e| StoreError::Invalid(format!("facts[{i}]: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod hint_tests {
    #[test]
    fn abandoned_and_reverted_suggest_a_dead_end_and_the_rest_do_not() {
        for outcome in ["abandoned", "reverted"] {
            let hint = super::dead_end_hint(outcome, "mem_a").unwrap();
            assert!(
                hint.contains("dead_end") && hint.contains("mem_a"),
                "{hint}"
            );
        }
        for outcome in ["done", "superseded", "carded"] {
            assert!(super::dead_end_hint(outcome, "mem_a").is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::NewMemory;
    use crate::db::Database;

    fn decision(store: &Store<'_>, id: &str) {
        let mut row = NewMemory::new(id, "we chose the engine", "2026-11-01T00:00:00Z");
        row.memory_type = "decision".into();
        row.metadata = json!({ "rationale": "benchmark" });
        Memories::new(store).insert(&row).unwrap();
    }

    fn revisions(store: &Store<'_>, id: &str) -> usize {
        crate::history::history(store, id, 100).unwrap().len()
    }

    /// The memory as `resolve` or `review` found it, and its history.
    fn unchanged(store: &Store<'_>, id: &str, before: usize) {
        let memory = Memories::new(store).get_live(id).unwrap().unwrap();
        assert_eq!(memory.outcome, None);
        assert!(memory.metadata.get("review").is_none());
        assert!(memory.metadata.get("outcome_note").is_none());
        assert_eq!(
            revisions(store, id),
            before,
            "no revision of a change that never happened"
        );
    }

    #[test]
    fn a_resolve_whose_edit_fails_leaves_no_revision() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        decision(&store, "d1");
        let before = revisions(&store, "d1");

        FAIL_EDIT.with(|fail| fail.set(true));
        assert!(resolve_memory(&store, "d1", "reverted", Some("backed out")).is_err());
        unchanged(&store, "d1", before);
    }

    #[test]
    fn a_review_whose_edit_fails_leaves_no_revision() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        decision(&store, "d1");
        let before = revisions(&store, "d1");

        FAIL_EDIT.with(|fail| fail.set(true));
        assert!(crate::review::record_review(&store, "d1", "reverted", None).is_err());
        unchanged(&store, "d1", before);
    }

    #[test]
    fn a_review_then_a_resolve_each_land_one_revision_with_their_edit() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        decision(&store, "d1");
        let before = revisions(&store, "d1");

        let reviewed = crate::review::record_review(&store, "d1", "held", None)
            .unwrap()
            .unwrap();
        assert!(reviewed.metadata.get("review").is_some());
        assert_eq!(revisions(&store, "d1"), before + 1);

        let resolved = resolve_memory(&store, "d1", "done", None).unwrap().unwrap();
        assert_eq!(resolved.outcome.as_deref(), Some("done"));
        assert_eq!(revisions(&store, "d1"), before + 2);
    }

    #[test]
    fn inside_a_failed_transaction_a_resolve_is_undone_with_it() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        decision(&store, "d1");
        let before = revisions(&store, "d1");

        let outer: Result<()> = store.transaction(|page| {
            resolve_memory(page, "d1", "abandoned", None)?;
            Err(StoreError::Invalid("caller gives up".into()))
        });
        assert!(outer.is_err());
        unchanged(&store, "d1", before);
    }
}
