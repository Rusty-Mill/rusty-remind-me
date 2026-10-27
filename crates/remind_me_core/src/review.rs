//! Decision follow-ups: a set time after a decision, ask whether it held up.
//!
//! [`crate::resolve`] records whether a decision was *carried out* (`done`,
//! `abandoned`, …). This module records whether it *held up*: `held`,
//! `revised` or `reverted`, found at a review scheduled when the decision is
//! written. Over time that is a track record of which choices were right,
//! not only what was chosen.
//!
//! # Stored in metadata, not columns
//!
//! The engine stores memory rows positionally, so a new column is a schema
//! bump and a data migration. The review lives in the decision's metadata
//! instead, which syncs and keeps history like any other metadata:
//!
//! ```json
//! { "stakes": "low" | "normal",
//!   "review": { "due": "<RFC 3339>", "outcome": "held" | "revised" | "reverted",
//!               "note": "…", "reviewed_at": "<RFC 3339>" } }
//! ```
//!
//! `kinds::validate_metadata` holds both to closed enums. The track-record
//! state ([`ReviewState`]) is derived from them and never stored, so it
//! cannot drift from what it summarizes.
//!
//! # Not a reminder
//!
//! `remind_at` is the memory's one reminder slot and belongs to the user.
//! Scheduling reviews into it would overwrite a reminder they set and fire
//! a desktop notification for every decision. The review has its own date;
//! the digest and the session brief surface it.

use crate::db::memories::{Memories, MemoryEdit};
use crate::db::{Result, Store, StoreError};
use crate::kinds::{MemoryKind, REVIEW_OUTCOMES};
use crate::models::Memory;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// Days after a decision its review falls due; `0` turns scheduling off.
pub const REVIEW_DAYS_ENV: &str = "REMIND_ME_DECISION_REVIEW_DAYS";
/// About three weeks: long enough for consequences to show.
pub const DEFAULT_REVIEW_DAYS: i64 = 21;
/// Ten years; a larger value is a typo rather than a plan.
pub const MAX_REVIEW_DAYS: i64 = 3650;
/// The revision reason a review leaves in history.
pub const REVIEW_REASON: &str = "review";
/// `memory_type` of the memories reviewed.
const DECISION: &str = "decision";

/// [`REVIEW_DAYS_ENV`], or the default when it is unset.
pub fn review_days() -> i64 {
    review_days_from(std::env::var(REVIEW_DAYS_ENV).ok().as_deref())
}

/// Parse a configured review interval. An unparseable or out-of-range value
/// falls back to the default, and says so: a typo must not silently turn
/// reviews off.
pub fn review_days_from(raw: Option<&str>) -> i64 {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return DEFAULT_REVIEW_DAYS;
    };
    match raw.parse::<i64>() {
        Ok(days) if (0..=MAX_REVIEW_DAYS).contains(&days) => days,
        _ => {
            eprintln!(
                "rusty-remind-me: {REVIEW_DAYS_ENV}={raw:?} is not a whole number of days \
                 from 0 to {MAX_REVIEW_DAYS}; using {DEFAULT_REVIEW_DAYS}"
            );
            DEFAULT_REVIEW_DAYS
        }
    }
}

/// What a review found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewOutcome {
    Held,
    Revised,
    Reverted,
}

impl ReviewOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Revised => "revised",
            Self::Reverted => "reverted",
        }
    }
}

impl fmt::Display for ReviewOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReviewOutcome {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "held" => Ok(Self::Held),
            "revised" => Ok(Self::Revised),
            "reverted" => Ok(Self::Reverted),
            _ => Err(StoreError::Invalid(format!(
                "`review` must be one of {}",
                REVIEW_OUTCOMES.join(", ")
            ))),
        }
    }
}

/// Where a decision stands in its follow-up. Derived, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    /// No review date: written before follow-ups existed, or with
    /// scheduling off.
    Unscheduled,
    /// `stakes: low`, never reviewed.
    LowStakes,
    /// Review date still ahead.
    Pending,
    /// Review date passed, no review yet.
    Due,
    Held,
    Revised,
    Reverted,
    /// Resolved as abandoned or superseded before any review: nothing left
    /// to hold up.
    Closed,
}

fn review_of(memory: &Memory) -> Option<&Value> {
    memory.metadata.get("review").filter(|r| r.is_object())
}

fn is_low_stakes(metadata: &Value) -> bool {
    metadata.get("stakes").and_then(Value::as_str) == Some("low")
}

/// The decision's review date, as stored.
pub fn review_due(memory: &Memory) -> Option<&str> {
    review_of(memory)?.get("due")?.as_str()
}

/// The review date as an instant. Compared parsed, never as text: an RFC 3339
/// string with an offset does not sort with one in UTC.
fn due_instant(memory: &Memory) -> Option<DateTime<Utc>> {
    let due = DateTime::parse_from_rfc3339(review_due(memory)?).ok()?;
    Some(due.with_timezone(&Utc))
}

/// The metadata a new decision should hold: `metadata` plus a review due
/// `days` after `from`. `None` when nothing should change: scheduling is
/// off, the decision is low-stakes, or it already has a review.
pub fn schedule(metadata: &Value, from: DateTime<Utc>, days: i64) -> Option<Value> {
    let has_review = metadata.get("review").is_some_and(|r| !r.is_null());
    if days == 0 || is_low_stakes(metadata) || has_review {
        return None;
    }
    let mut scheduled = match metadata {
        Value::Object(_) => metadata.clone(),
        _ => json!({}),
    };
    scheduled["review"] = json!({ "due": (from + Duration::days(days)).to_rfc3339() });
    Some(scheduled)
}

/// [`schedule`] with the configured interval, from now, for a memory that is
/// becoming `memory_type`. `None` for anything but a decision.
pub fn schedule_if_decision(memory_type: Option<&str>, metadata: &Value) -> Option<Value> {
    if memory_type != Some(DECISION) {
        return None;
    }
    schedule(metadata, Utc::now(), review_days())
}

/// Where `memory`, a decision, stands as of `now`.
pub fn state(memory: &Memory, now: DateTime<Utc>) -> ReviewState {
    let review = review_of(memory);
    match review
        .and_then(|r| r.get("outcome"))
        .and_then(Value::as_str)
    {
        Some("held") => return ReviewState::Held,
        Some("revised") => return ReviewState::Revised,
        Some("reverted") => return ReviewState::Reverted,
        _ => {}
    }
    match memory.outcome.as_deref() {
        Some("reverted") => return ReviewState::Reverted,
        Some("abandoned" | "superseded") => return ReviewState::Closed,
        _ => {}
    }
    if is_low_stakes(&memory.metadata) {
        return ReviewState::LowStakes;
    }
    match due_instant(memory) {
        Some(due) if due <= now => ReviewState::Due,
        Some(_) => ReviewState::Pending,
        None => ReviewState::Unscheduled,
    }
}

/// The decisions among `memories` due for review as of `now`, in `project`
/// when given, oldest due date first (ties by id). Only what the session
/// brief would inject unasked is listed: not sensitive, superseded, expired
/// or a raw dialog.
pub fn due_among<'a>(
    memories: &'a [Memory],
    now: DateTime<Utc>,
    project: Option<&str>,
) -> Vec<&'a Memory> {
    let scope = crate::context::ScopeFilter::new(project.map(str::to_string), None, None, None);
    let mut due: Vec<&Memory> = memories
        .iter()
        .filter(|m| m.memory_type.as_deref() == Some(DECISION))
        .filter(|m| crate::context_brief::injectable(m, now) && scope.matches_memory(m))
        .filter(|m| state(m, now) == ReviewState::Due)
        .collect();
    due.sort_by(|a, b| (due_instant(a), &a.id).cmp(&(due_instant(b), &b.id)));
    due
}

/// Decisions due for review now, at most `limit`; see [`due_among`].
pub fn list_due(store: &Store<'_>, project: Option<&str>, limit: usize) -> Result<Vec<Memory>> {
    let decisions = Memories::new(store).live_of_type(DECISION)?;
    Ok(due_among(&decisions, Utc::now(), project)
        .into_iter()
        .take(limit)
        .cloned()
        .collect())
}

/// Record that decision `memory_id` was reviewed and `outcome` found, with
/// an optional `note`. `None` when no live memory has the id.
///
/// A review may be recorded before its due date, on a decision that never
/// had one, or again to correct an earlier one; history keeps each. A
/// `reverted` review also resolves the decision as `reverted`, with the
/// demotion that carries, since it was undone either way.
pub fn record_review(
    store: &Store<'_>,
    memory_id: &str,
    outcome: &str,
    note: Option<&str>,
) -> Result<Option<Memory>> {
    let outcome: ReviewOutcome = outcome.parse()?;
    let Some(memory) = Memories::new(store).get_live(memory_id)? else {
        return Ok(None);
    };
    let kind = memory
        .memory_type
        .as_deref()
        .and_then(|t| t.parse::<MemoryKind>().ok())
        .unwrap_or(MemoryKind::Unclassified);
    if kind != MemoryKind::Decision {
        return Err(StoreError::Invalid(format!(
            "`review` only applies to decision memories, not {kind}"
        )));
    }

    let now = Utc::now().to_rfc3339();
    crate::resolve::record_revision(store, memory_id, &now, REVIEW_REASON)?;

    let newly_reverted =
        outcome == ReviewOutcome::Reverted && memory.outcome.as_deref() != Some("reverted");
    Memories::new(store).apply_edit(
        memory_id,
        &MemoryEdit {
            metadata: Some(with_review(&memory.metadata, outcome, note, &now)),
            outcome: newly_reverted.then(|| "reverted".to_string()),
            ..MemoryEdit::at(now.as_str())
        },
    )?;
    if newly_reverted {
        crate::resolve::demote(store, memory_id)?;
    }
    Memories::new(store).get_live(memory_id)
}

/// `metadata` with its review completed. A missing due date becomes `now`;
/// a note from an earlier review is replaced, or dropped when none is given,
/// so it can never describe a different finding.
fn with_review(metadata: &Value, outcome: ReviewOutcome, note: Option<&str>, now: &str) -> Value {
    let mut metadata = match metadata {
        Value::Object(_) => metadata.clone(),
        _ => json!({}),
    };
    let mut review = metadata
        .get("review")
        .filter(|r| r.is_object())
        .cloned()
        .unwrap_or_else(|| json!({ "due": now }));
    review["outcome"] = json!(outcome.as_str());
    review["reviewed_at"] = json!(now);
    match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(note) => review["note"] = json!(note),
        None => {
            if let Some(r) = review.as_object_mut() {
                r.remove("note");
            }
        }
    }
    metadata["review"] = review;
    metadata
}

/// Decisions by review state: overall, and per project and month written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionReport {
    pub total: usize,
    pub by_state: BTreeMap<ReviewState, usize>,
    /// Project (alphabetical, unscoped last), then month (oldest first).
    pub by_period: Vec<DecisionPeriod>,
}

/// One project's decisions written in one month.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionPeriod {
    pub project: Option<String>,
    /// `YYYY-MM` of `created_at`.
    pub month: String,
    pub by_state: BTreeMap<ReviewState, usize>,
}

/// The report over every live decision in the store.
pub fn report(store: &Store<'_>) -> Result<DecisionReport> {
    let decisions = Memories::new(store).live_of_type(DECISION)?;
    Ok(report_of(&decisions, Utc::now()))
}

/// The report over `decisions` as of `now`.
pub fn report_of(decisions: &[Memory], now: DateTime<Utc>) -> DecisionReport {
    let mut by_state = BTreeMap::new();
    // `(None, _)` sorts first; flipped below so unscoped decisions come last.
    let mut periods: BTreeMap<(bool, Option<String>, String), BTreeMap<ReviewState, usize>> =
        BTreeMap::new();
    for decision in decisions {
        let state = state(decision, now);
        *by_state.entry(state).or_insert(0) += 1;
        let month: String = decision.created_at.chars().take(7).collect();
        let key = (decision.project.is_none(), decision.project.clone(), month);
        *periods.entry(key).or_default().entry(state).or_insert(0) += 1;
    }
    DecisionReport {
        total: decisions.len(),
        by_state,
        by_period: periods
            .into_iter()
            .map(|((_, project, month), by_state)| DecisionPeriod {
                project,
                month,
                by_state,
            })
            .collect(),
    }
}

/// Render the report as a Markdown section, for `remind_me_stats`.
pub fn render_markdown(report: &DecisionReport) -> String {
    let mut out = String::from("### Decision follow-ups\n");
    if report.total == 0 {
        out.push_str("_No decisions recorded._\n");
        return out;
    }
    let counts = |by_state: &BTreeMap<ReviewState, usize>| {
        by_state
            .iter()
            .map(|(state, n)| format!("{}: {n}", state_label(*state)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    out.push_str(&format!(
        "- {} decisions — {}\n",
        report.total,
        counts(&report.by_state)
    ));
    for period in &report.by_period {
        out.push_str(&format!(
            "- {} {}: {}\n",
            period.project.as_deref().unwrap_or("(no project)"),
            period.month,
            counts(&period.by_state)
        ));
    }
    out
}

fn state_label(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Unscheduled => "unscheduled",
        ReviewState::LowStakes => "low_stakes",
        ReviewState::Pending => "pending",
        ReviewState::Due => "due",
        ReviewState::Held => "held",
        ReviewState::Revised => "revised",
        ReviewState::Reverted => "reverted",
        ReviewState::Closed => "closed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::NewMemory;
    use crate::db::Database;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    const NOW: &str = "2026-10-26T12:00:00+00:00";

    fn decision(id: &str, metadata: Value) -> Memory {
        let mut row = NewMemory::new(id, format!("we chose {id}"), NOW);
        row.memory_type = DECISION.into();
        row.metadata = metadata;
        crate::db::engine::memories::MemoryRow::from_new(&row).to_memory()
    }

    fn insert(store: &Store<'_>, id: &str, f: impl FnOnce(&mut NewMemory)) {
        let mut row = NewMemory::new(id, format!("we chose {id}"), &Utc::now().to_rfc3339());
        row.category = DECISION.into();
        row.memory_type = DECISION.into();
        row.metadata = json!({ "rationale": "cheaper" });
        f(&mut row);
        Memories::new(store).insert(&row).unwrap();
    }

    fn past() -> Value {
        json!({ "rationale": "r", "review": { "due": "2026-01-01T00:00:00+00:00" } })
    }

    #[test]
    fn review_days_defaults_parses_and_rejects() {
        assert_eq!(review_days_from(None), DEFAULT_REVIEW_DAYS);
        assert_eq!(review_days_from(Some(" ")), DEFAULT_REVIEW_DAYS);
        assert_eq!(review_days_from(Some("7")), 7);
        assert_eq!(review_days_from(Some("0")), 0);
        assert_eq!(review_days_from(Some("-1")), DEFAULT_REVIEW_DAYS);
        assert_eq!(review_days_from(Some("3651")), DEFAULT_REVIEW_DAYS);
        assert_eq!(review_days_from(Some("soon")), DEFAULT_REVIEW_DAYS);
    }

    #[test]
    fn schedule_adds_a_due_date_days_later() {
        let scheduled = schedule(&json!({ "rationale": "r" }), at(NOW), 21).unwrap();
        assert_eq!(scheduled["rationale"], "r");
        assert_eq!(scheduled["review"]["due"], "2026-11-16T12:00:00+00:00");
        assert!(crate::kinds::validate_metadata(MemoryKind::Decision, &scheduled).is_ok());
    }

    #[test]
    fn schedule_skips_off_low_stakes_and_already_scheduled() {
        assert_eq!(schedule(&json!({}), at(NOW), 0), None);
        assert_eq!(schedule(&json!({ "stakes": "low" }), at(NOW), 21), None);
        assert_eq!(schedule(&past(), at(NOW), 21), None);
        assert!(schedule(&json!({ "stakes": "normal" }), at(NOW), 21).is_some());
        assert!(schedule(&Value::Null, at(NOW), 21).is_some());
    }

    #[test]
    fn schedule_if_decision_ignores_other_kinds() {
        assert_eq!(schedule_if_decision(Some("fact"), &json!({})), None);
        assert_eq!(schedule_if_decision(None, &json!({})), None);
    }

    #[test]
    fn state_covers_every_case() {
        let now = at(NOW);
        let cases = [
            (json!({}), None, ReviewState::Unscheduled),
            (json!({ "stakes": "low" }), None, ReviewState::LowStakes),
            (
                json!({ "review": { "due": "2026-12-01T00:00:00Z" } }),
                None,
                ReviewState::Pending,
            ),
            (past(), None, ReviewState::Due),
            (past(), Some("done"), ReviewState::Due),
            (past(), Some("abandoned"), ReviewState::Closed),
            (past(), Some("superseded"), ReviewState::Closed),
            (past(), Some("reverted"), ReviewState::Reverted),
        ];
        for (metadata, outcome, want) in cases {
            let mut m = decision("d", metadata.clone());
            m.outcome = outcome.map(str::to_string);
            assert_eq!(state(&m, now), want, "{metadata} / {outcome:?}");
        }
        for (found, want) in [
            ("held", ReviewState::Held),
            ("revised", ReviewState::Revised),
            ("reverted", ReviewState::Reverted),
        ] {
            let m = decision(
                "d",
                json!({ "review": { "due": "2026-12-01T00:00:00Z", "outcome": found } }),
            );
            assert_eq!(state(&m, now), want);
        }
    }

    #[test]
    fn due_among_filters_scopes_and_orders() {
        let now = at(NOW);
        let later = json!({ "review": { "due": "2026-02-01T00:00:00+00:00" } });
        let mut other = decision("other_project", past());
        other.project = Some("elsewhere".into());
        let mut secret = decision("secret", past());
        secret.sensitive = true;
        let mut replaced = decision("replaced", past());
        replaced.superseded_by = Some("x".into());
        let mut fact = decision("fact", past());
        fact.memory_type = Some("fact".into());
        let mut b = decision("b_later", later);
        b.project = Some("quokka".into());
        let mut a = decision("a_earlier", past());
        a.project = Some("quokka".into());
        let pending = decision(
            "pending",
            json!({ "review": { "due": "2027-01-01T00:00:00Z" } }),
        );
        let mut expired = decision("expired", past());
        expired.valid_until = Some("2026-03-01T00:00:00+00:00".into());
        // 13:00+02:00 is 11:00Z, an hour before NOW, though it sorts after.
        let mut offset_expired = decision("offset_expired", past());
        offset_expired.valid_until = Some("2026-10-26T13:00:00+02:00".into());
        let mut dialog = decision("dialog", past());
        dialog.category = crate::models::DIALOG_CATEGORY.into();
        let all = [
            other,
            secret,
            replaced,
            fact,
            b,
            a,
            pending,
            expired,
            offset_expired,
            dialog,
        ];

        let ids = |v: Vec<&Memory>| v.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(due_among(&all, now, Some("quokka"))),
            ["a_earlier", "b_later"]
        );
        assert_eq!(
            ids(due_among(&all, now, None)),
            ["a_earlier", "other_project", "b_later"]
        );
    }

    #[test]
    fn due_dates_sort_as_instants_not_text() {
        // 09:00+09:00 is 00:00Z, before 01:00Z; as text it sorts after.
        let offset = decision(
            "z_offset",
            json!({ "review": { "due": "2026-01-02T09:00:00+09:00" } }),
        );
        let utc = decision(
            "a_utc",
            json!({ "review": { "due": "2026-01-02T01:00:00Z" } }),
        );
        let tie = decision(
            "b_tie",
            json!({ "review": { "due": "2026-01-02T00:00:00+00:00" } }),
        );
        let all = [utc, offset, tie];
        let ids: Vec<&str> = due_among(&all, at(NOW), None)
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, ["b_tie", "z_offset", "a_utc"]);
    }

    #[test]
    fn list_due_reads_the_store() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        insert(&store, "due", |r| r.metadata = past());
        insert(&store, "fresh", |_| {});
        insert(&store, "gone", |r| r.metadata = past());
        crate::db::queries::delete_memory(&store, "gone").unwrap();
        let due = list_due(&store, None, 10).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "due");
        assert!(list_due(&store, None, 0).unwrap().is_empty());
    }

    #[test]
    fn each_review_outcome_is_recorded() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        for outcome in REVIEW_OUTCOMES {
            insert(&store, outcome, |r| r.metadata = past());
            let m = record_review(&store, outcome, outcome, Some("  saw it  "))
                .unwrap()
                .unwrap();
            let review = &m.metadata["review"];
            assert_eq!(review["outcome"], outcome);
            assert_eq!(review["note"], "saw it");
            assert_eq!(review["due"], "2026-01-01T00:00:00+00:00");
            assert!(review["reviewed_at"].is_string());
            assert_eq!(m.metadata["rationale"], "r");
            assert!(crate::kinds::validate_metadata(MemoryKind::Decision, &m.metadata).is_ok());
            assert!(list_due(&store, None, 10)
                .unwrap()
                .iter()
                .all(|d| d.id != outcome));
        }
    }

    #[test]
    fn held_and_revised_leave_execution_alone() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        insert(&store, "d", |r| r.outcome = Some("done".into()));
        let before = Memories::new(&store).get_live("d").unwrap().unwrap();
        let m = record_review(&store, "d", "held", None).unwrap().unwrap();
        assert_eq!(m.outcome.as_deref(), Some("done"));
        assert_eq!(m.base_weight, before.base_weight);
        let m = record_review(&store, "d", "revised", None)
            .unwrap()
            .unwrap();
        assert_eq!(m.outcome.as_deref(), Some("done"));
    }

    #[test]
    fn reverted_resolves_and_demotes_once() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        insert(&store, "d", |_| {});
        let before = Memories::new(&store).get_live("d").unwrap().unwrap();
        let m = record_review(&store, "d", "reverted", None)
            .unwrap()
            .unwrap();
        assert_eq!(m.outcome.as_deref(), Some("reverted"));
        assert!(m.base_weight < before.base_weight);
        let again = record_review(&store, "d", "reverted", None)
            .unwrap()
            .unwrap();
        assert_eq!(again.base_weight, m.base_weight);
    }

    #[test]
    fn a_review_without_a_date_or_note_gets_one_and_drops_the_old_note() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        insert(&store, "d", |_| {});
        record_review(&store, "d", "held", Some("first")).unwrap();
        let m = record_review(&store, "d", "revised", None)
            .unwrap()
            .unwrap();
        let review = &m.metadata["review"];
        assert!(review["due"].is_string());
        assert_eq!(review["outcome"], "revised");
        assert!(review.get("note").is_none());
    }

    #[test]
    fn a_review_keeps_history() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        insert(&store, "d", |r| r.metadata = past());
        record_review(&store, "d", "held", None).unwrap();
        let revisions = crate::history::history(&store, "d", 10).unwrap();
        assert!(!revisions.is_empty());
    }

    #[test]
    fn record_review_refuses_bad_input() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        insert(&store, "d", |_| {});
        insert(&store, "f", |r| r.memory_type = "fact".into());
        let e = record_review(&store, "d", "fine", None).unwrap_err();
        assert!(e.to_string().contains("held, revised, reverted"), "{e}");
        let e = record_review(&store, "f", "held", None).unwrap_err();
        assert!(e.to_string().contains("not fact"), "{e}");
        assert!(record_review(&store, "missing", "held", None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn report_counts_by_state_project_and_month() {
        let now = at(NOW);
        let mk = |id: &str, project: Option<&str>, created: &str, metadata: Value| {
            let mut m = decision(id, metadata);
            m.project = project.map(str::to_string);
            m.created_at = created.into();
            m
        };
        let held = json!({ "review": { "due": "2026-01-01T00:00:00Z", "outcome": "held" } });
        let all = [
            mk("a", Some("quokka"), "2026-09-03T00:00:00Z", held.clone()),
            mk("b", Some("quokka"), "2026-09-20T00:00:00Z", past()),
            mk("c", Some("quokka"), "2026-10-01T00:00:00Z", held),
            mk("d", None, "2026-10-02T00:00:00Z", json!({})),
            mk(
                "e",
                Some("alpha"),
                "2026-10-05T00:00:00Z",
                json!({ "stakes": "low" }),
            ),
        ];
        let report = report_of(&all, now);
        assert_eq!(report.total, 5);
        assert_eq!(report.by_state[&ReviewState::Held], 2);
        assert_eq!(report.by_state[&ReviewState::Due], 1);
        let rows: Vec<(Option<&str>, &str)> = report
            .by_period
            .iter()
            .map(|p| (p.project.as_deref(), p.month.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                (Some("alpha"), "2026-10"),
                (Some("quokka"), "2026-09"),
                (Some("quokka"), "2026-10"),
                (None, "2026-10"),
            ]
        );
        assert_eq!(report.by_period[1].by_state.len(), 2);

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["by_state"]["low_stakes"], 1);
        let back: DecisionReport = serde_json::from_value(json).unwrap();
        assert_eq!(back, report);

        let md = render_markdown(&report);
        assert!(md.contains("- 5 decisions"), "{md}");
        assert!(md.contains("(no project) 2026-10: unscheduled: 1"), "{md}");
    }

    #[test]
    fn an_empty_vault_reports_nothing() {
        let report = report_of(&[], at(NOW));
        assert_eq!(report, DecisionReport::default());
        assert!(render_markdown(&report).contains("No decisions recorded"));
    }

    fn add(store: &Store<'_>, memory_type: &str, metadata: Value) -> Memory {
        let input =
            serde_json::from_value(json!({ "content": "pick X", "metadata": metadata })).unwrap();
        crate::db::queries::add_memory_with(
            store,
            input,
            &crate::kinds::StructuredFields {
                memory_type: Some(memory_type.into()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn a_new_decision_is_scheduled_and_a_low_stakes_one_is_not() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let d = add(&store, DECISION, json!({ "rationale": "r" }));
        assert_eq!(state(&d, Utc::now()), ReviewState::Pending);
        let due = DateTime::parse_from_rfc3339(review_due(&d).unwrap()).unwrap();
        let days = (due.with_timezone(&Utc) - Utc::now()).num_days();
        assert!((DEFAULT_REVIEW_DAYS - 1..=DEFAULT_REVIEW_DAYS).contains(&days));

        let low = add(
            &store,
            DECISION,
            json!({ "rationale": "r", "stakes": "low" }),
        );
        assert!(low.metadata.get("review").is_none());
        let fact = add(&store, "fact", json!({}));
        assert!(fact.metadata.get("review").is_none());
        let kept = add(&store, DECISION, past());
        assert_eq!(review_due(&kept), Some("2026-01-01T00:00:00+00:00"));
    }

    #[test]
    fn becoming_a_decision_schedules_and_staying_one_does_not() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let fact = add(&store, "fact", json!({ "rationale": "r" }));
        let update = |id: &str, metadata: Option<Value>| {
            let input =
                serde_json::from_value(json!({ "memory_id": id, "metadata": metadata })).unwrap();
            crate::db::queries::update_memory_with(
                &store,
                &input,
                &crate::kinds::StructuredFields {
                    memory_type: Some(DECISION.into()),
                    ..Default::default()
                },
            )
            .unwrap();
            Memories::new(&store).get_live(id).unwrap().unwrap()
        };
        let became = update(&fact.id, None);
        assert!(review_due(&became).is_some());

        // An edit to a decision that is already one leaves its review alone.
        let d = add(&store, DECISION, past());
        let edited = update(&d.id, Some(past()));
        assert_eq!(review_due(&edited), Some("2026-01-01T00:00:00+00:00"));

        let other = add(&store, "fact", json!({ "rationale": "r" }));
        crate::db::queries::reclassify_memories(
            &store,
            &crate::models::ReclassifyInput {
                classifications: vec![crate::models::MemoryClassification {
                    memory_id: other.id.clone(),
                    memory_type: DECISION.into(),
                }],
            },
        )
        .unwrap();
        let reclassified = Memories::new(&store).get_live(&other.id).unwrap().unwrap();
        assert!(review_due(&reclassified).is_some());
        assert_eq!(reclassified.metadata["rationale"], "r");
    }

    #[test]
    fn invalid_review_metadata_is_refused() {
        use crate::kinds::validate_metadata;
        let k = MemoryKind::Decision;
        for (bad, field) in [
            (json!({ "rationale": "r", "stakes": "huge" }), "stakes"),
            (json!({ "rationale": "r", "review": "soon" }), "review"),
            (json!({ "rationale": "r", "review": {} }), "review.due"),
            (
                json!({ "rationale": "r", "review": { "due": "later" } }),
                "review.due",
            ),
            (
                json!({ "rationale": "r", "review": { "due": "2026-01-01T00:00:00Z", "outcome": "fine" } }),
                "review.outcome",
            ),
            (
                json!({ "rationale": "r", "review": { "due": "2026-01-01T00:00:00Z", "reviewed_at": "x" } }),
                "review.reviewed_at",
            ),
            (
                json!({ "rationale": "r", "review": { "due": "2026-01-01T00:00:00Z", "note": 3 } }),
                "review.note",
            ),
        ] {
            let e = validate_metadata(k, &bad).unwrap_err().to_string();
            assert!(e.contains(&format!("`{field}`")), "{bad}: {e}");
        }
        assert!(validate_metadata(k, &json!({ "rationale": "r", "review": null })).is_ok());
    }

    #[test]
    fn due_reviews_reach_the_digest_and_the_brief() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        insert(&store, "due_here", |r| {
            r.metadata = past();
            r.project = Some("quokka".into());
        });
        insert(&store, "due_elsewhere", |r| {
            r.metadata = past();
            r.project = Some("elsewhere".into());
        });
        insert(&store, "not_yet", |r| r.project = Some("quokka".into()));
        insert(&store, "expired_here", |r| {
            r.metadata = past();
            r.project = Some("quokka".into());
            r.valid_until = Some("2026-03-01T00:00:00+00:00".into());
        });
        // Expired an hour before the real now, in a later-looking offset.
        let offset_expired = (Utc::now() - Duration::hours(1))
            .with_timezone(&chrono::FixedOffset::east_opt(2 * 3600).unwrap())
            .to_rfc3339();
        insert(&store, "offset_expired_here", |r| {
            r.metadata = past();
            r.project = Some("quokka".into());
            r.valid_until = Some(offset_expired);
        });
        insert(&store, "dialog_here", |r| {
            r.metadata = past();
            r.project = Some("quokka".into());
            r.category = crate::models::DIALOG_CATEGORY.into();
        });

        let digest = crate::digest::build_digest(&store, 7).unwrap();
        let ids: Vec<&str> = digest.reviews_due.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["due_elsewhere", "due_here"]);
        assert_eq!(digest.reviews_due[0].due, "2026-01-01T00:00:00+00:00");
        let md = crate::digest::render_markdown(&digest);
        assert!(
            md.contains("## Decisions to review") && md.contains("`due_here`"),
            "{md}"
        );

        let request = crate::context_brief::ContextRequest {
            project: Some("quokka"),
            budget: crate::context_brief::DEFAULT_TOKEN_BUDGET,
            ..Default::default()
        };
        let crate::context_brief::Outcome::Brief(brief) =
            crate::context_brief::build(&store, &request).unwrap()
        else {
            panic!("quokka is a known project");
        };
        let context = crate::context_brief::render_markdown(&brief);
        let section = |name: &str| brief.sections.iter().find(|s| s.name == name).unwrap();
        let reviews: Vec<&str> = section("reviews")
            .items
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(reviews, ["due_here"], "{context}");
        assert!(context.contains("Decisions to review"), "{context}");
        assert!(
            !section("open").items.iter().any(|i| i.id == "due_here"),
            "a due review is listed once"
        );
        assert!(!context.contains("due_elsewhere"), "{context}");

        record_review(&store, "due_here", "held", None).unwrap();
        let digest = crate::digest::build_digest(&store, 7).unwrap();
        assert_eq!(digest.reviews_due.len(), 1);
    }

    #[test]
    fn an_empty_digest_says_none_are_due() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let digest = crate::digest::build_digest(&store, 7).unwrap();
        assert!(crate::digest::render_markdown(&digest).contains("_None due._"));
    }
}
