//! Saved and watched searches.
//!
//! A saved search stores a query plus its filters under a unique name, so a
//! recurring question does not have to be retyped with the same filters every
//! time. `watch` marks one for polling: [`poll_saved_search`] reports matches
//! that have not been seen before, which is what `saved_search_seen_memories`
//! records.
//!
//! # What "watch" does and does not mean here
//!
//! Running a saved search returns **all** its current matches, watched or not.
//! That is the reference's behaviour and it is worth being explicit about,
//! because the obvious guess is the opposite: `remind_me_run_saved_search`
//! calls the search core and returns whatever it returns. The unseen-only diff
//! belongs to polling, not to running — asking for a saved search's results
//! and getting a partial list because something polled it earlier would be
//! surprising and unfixable from the caller's side.
//!
//! [`poll_saved_search`] computes the diff and records it. Delivering it —
//! notification channels — is the scheduler's half and lands with issue #117;
//! this module deliberately returns the new matches rather than dispatching
//! them, so the logic is complete and testable before any transport exists.
//!
//! # Seeding
//!
//! The first poll of a saved search records every current match as seen and
//! reports **none of them**. Turning on `watch` for a search that already
//! matches a hundred memories must not read as a hundred new matches, because
//! none of them are new — the watch started now.

use crate::db::queries::search_memories;
use crate::db::saved_searches::SavedSearches;
use crate::db::Result;
use crate::db::Store;
use crate::models::{
    AnswerPinnedInput, MemorySearchInput, PinnedAnswer, SaveSearchInput, SavedSearch,
    SavedSearchFilters, PIN_EVIDENCE_MAX, POLL_RESULT_LIMIT,
};
use crate::staleness::{stale_check, NEW_EVIDENCE_MAX};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Stable id for a saved search, derived from its name.
///
/// Content-derived rather than random so that re-saving the same name is
/// recognisably the same row even if a caller has cached the id.
fn make_id(name: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    format!("ss_{:016x}", hasher.finish())
}

/// Create a saved search, or update it in place when the name already exists.
///
/// Update-by-name rather than a duplicate insert: re-saving under the same
/// name is how a caller changes a saved search's query, filters or watch flag.
/// The same "one name is one logical thing" convention `remind_me_wiki_write`
/// already uses for pages, and what the table's `UNIQUE` on `name` implies.
pub fn save_search(store: &Store<'_>, input: &SaveSearchInput) -> Result<SavedSearch> {
    let repo = SavedSearches::new(store);
    let now = Utc::now().to_rfc3339();
    let existing = repo.id_for_name(&input.name)?;
    // Re-saving keeps a pinned question's answer (#61); a changed query
    // then reads as stale until it is answered again.
    let pin = match &existing {
        Some(id) => repo.get(id)?.filters.pin,
        None => None,
    };
    let saved = SavedSearch {
        id: existing.clone().unwrap_or_else(|| make_id(&input.name)),
        name: input.name.clone(),
        query: input.query.clone(),
        filters: SavedSearchFilters {
            category: input.category.clone(),
            tags: input.tags.clone(),
            include_sensitive: input.include_sensitive,
            pin,
        },
        watch: input.watch,
        created_at: now.clone(),
        updated_at: now,
    };
    match existing {
        Some(_) => repo.update(&saved)?,
        None => repo.insert(&saved)?,
    }
    repo.get(&saved.id)
}

/// Every saved search, alphabetical by name.
pub fn list_saved_searches(store: &Store<'_>) -> Result<Vec<SavedSearch>> {
    SavedSearches::new(store).list()
}

/// One saved search by name, or `None`.
pub fn get_saved_search(store: &Store<'_>, name: &str) -> Result<Option<SavedSearch>> {
    SavedSearches::new(store).get_by_name(name)
}

/// Delete a saved search and its seen-memory rows. `false` if no saved
/// search has that name.
pub fn delete_saved_search(store: &Store<'_>, name: &str) -> Result<bool> {
    let repo = SavedSearches::new(store);
    let Some(id) = repo.id_for_name(name)? else {
        return Ok(false);
    };
    repo.delete(&id)?;
    Ok(true)
}

/// The `MemorySearchInput` a saved search's stored query and filters imply.
///
/// `limit` and `token_budget` are overridable so polling can ask for a wider,
/// untruncated result set without those choices leaking into what a caller
/// gets from running the search by hand.
pub fn build_search_input(saved: &SavedSearch, limit: Option<usize>) -> MemorySearchInput {
    let mut input = MemorySearchInput {
        query: saved.query.clone(),
        category: saved.filters.category.clone(),
        tags: saved.filters.tags.clone(),
        include_sensitive: saved.filters.include_sensitive,
        ..Default::default()
    };
    if let Some(limit) = limit {
        input.limit = limit;
        // A poll diffs result sets, so a token budget that truncates the tail
        // would make dropped results look like they stopped matching.
        input.token_budget = usize::MAX;
    }
    // Dormant memories are still matches: a watch that stopped reporting a
    // memory because it decayed would look like the memory had been deleted.
    input.include_dormant = true;
    input
}

/// Run a saved search and return its matches — **all** of them, watched or
/// not. See the module docs for why watching does not narrow this.
pub fn run_saved_search(
    store: &Store<'_>,
    saved: &SavedSearch,
) -> Result<Vec<crate::models::MemorySearchResult>> {
    search_memories(store, &build_search_input(saved, None))
}

/// What one poll of a watched search found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollOutcome {
    /// Memory ids matching now that had not been seen before.
    pub new_matches: Vec<String>,
    /// True when this was the first poll and the current matches were
    /// recorded without being reported.
    pub seeded: bool,
}

/// Poll one watched saved search: record its current matches and report the
/// ones that are new.
///
/// The first poll **seeds** — every current match is recorded and none
/// reported — because turning on `watch` for a search that already matches is
/// not the same as those memories having just appeared.
///
/// Returns the new matches rather than dispatching notifications. The
/// transport is the scheduler's half (#117); keeping it out of here means the
/// diff logic is complete and testable without one.
pub fn poll_saved_search(store: &Store<'_>, saved: &SavedSearch) -> Result<PollOutcome> {
    let results = search_memories(store, &build_search_input(saved, Some(POLL_RESULT_LIMIT)))?;
    let current: Vec<String> = results.into_iter().map(|r| r.memory.id).collect();
    let repo = SavedSearches::new(store);
    let now = Utc::now().to_rfc3339();

    if !repo.has_any_seen(&saved.id)? {
        repo.mark_seen(&saved.id, &current, &now)?;
        return Ok(PollOutcome {
            new_matches: Vec::new(),
            seeded: true,
        });
    }

    let already = repo.seen_ids(&saved.id)?;
    let new_matches: Vec<String> = current
        .iter()
        .filter(|id| !already.contains(*id))
        .cloned()
        .collect();

    repo.mark_seen(&saved.id, &current, &now)?;
    Ok(PollOutcome {
        new_matches,
        seeded: false,
    })
}

/// Poll every watched saved search once.
pub fn poll_watched_searches(store: &Store<'_>) -> Result<Vec<(String, PollOutcome)>> {
    let watched: Vec<SavedSearch> = list_saved_searches(store)?
        .into_iter()
        .filter(|s| s.watch)
        .collect();

    let mut outcomes = Vec::with_capacity(watched.len());
    for saved in watched {
        let outcome = poll_saved_search(store, &saved)?;
        outcomes.push((saved.name, outcome));
    }
    Ok(outcomes)
}

// -- pinned questions (card #61, D84, D89) -----------------------------------

/// Mark the saved search `name` as a pinned question. `None` if no saved
/// search has that name. Pinning an already pinned search keeps its answer.
pub fn pin_saved_search(store: &Store<'_>, name: &str) -> Result<Option<SavedSearch>> {
    let repo = SavedSearches::new(store);
    let Some(mut saved) = repo.get_by_name(name)? else {
        return Ok(None);
    };
    if saved.filters.pin.is_none() {
        saved.filters.pin = Some(PinnedAnswer::default());
        saved.updated_at = Utc::now().to_rfc3339();
        repo.update(&saved)?;
    }
    repo.get(&saved.id).map(Some)
}

/// Record a pinned question's answer: the wiki page and the live memories
/// it was written from. Pins the search if it was not pinned. The server
/// stamps `answered_at`; the query is recorded with it.
///
/// # Errors
///
/// [`StoreError::Invalid`] when the saved search or the page does not
/// exist, no or too many ids are given, or an id is not a live memory.
pub fn answer_pinned(store: &Store<'_>, input: &AnswerPinnedInput) -> Result<SavedSearch> {
    let repo = SavedSearches::new(store);
    let Some(mut saved) = repo.get_by_name(&input.name)? else {
        return Err(invalid(format!(
            "no saved search is named {:?}",
            input.name
        )));
    };
    if crate::wiki::get_wiki_page(store, &input.answer_slug)?.is_none() {
        return Err(invalid(format!(
            "no wiki page has slug {:?}",
            input.answer_slug
        )));
    }
    let evidence = dedup(&input.evidence_ids);
    if evidence.is_empty() || evidence.len() > PIN_EVIDENCE_MAX {
        return Err(invalid(format!(
            "evidence_ids needs 1 to {PIN_EVIDENCE_MAX} memory ids, got {}",
            evidence.len()
        )));
    }
    let live = live_ids(store, &evidence)?;
    if let Some(gone) = evidence.iter().find(|id| !live.contains(*id)) {
        return Err(invalid(format!("{gone:?} is not a live memory")));
    }

    let now = Utc::now().to_rfc3339();
    saved.filters.pin = Some(PinnedAnswer {
        answer_slug: Some(input.answer_slug.clone()),
        evidence_ids: evidence,
        answered_at: Some(now.clone()),
        answered_query: Some(saved.query.clone()),
    });
    saved.updated_at = now;
    repo.update(&saved)?;
    repo.get(&saved.id)
}

/// Why a pinned question needs a new answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    /// Pinned but never answered.
    Unanswered,
    /// The saved query changed since the answer was written.
    QueryChanged,
    /// The answer's wiki page no longer exists.
    AnswerMissing,
    /// A memory the answer was written from is deleted or superseded.
    EvidenceGone,
    /// The query now returns memories written or changed since the answer.
    NewResults,
    /// Newer memories share an entity with the evidence ([`stale_check`]).
    RelatedEvidence,
}

/// A pinned question whose answer is out of date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalePinned {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_slug: Option<String>,
    pub reasons: Vec<StaleReason>,
    /// Up to [`NEW_EVIDENCE_MAX`] memories to read before answering again,
    /// newest results first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub new_evidence: Vec<String>,
    /// Evidence ids that are now deleted or superseded (D94). A superseding
    /// memory is not followed: it shows only if the query matches it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_gone: Vec<String>,
}

/// Every pinned question that needs a new answer, by name.
///
/// Phase one of the two-phase refresh: the client reads the new evidence,
/// writes the answer page and records it with [`answer_pinned`]. Nothing is
/// locked in between; evidence that lands meanwhile shows on the next call.
pub fn stale_pinned(store: &Store<'_>) -> Result<Vec<StalePinned>> {
    let now = Utc::now();
    let mut out = Vec::new();
    for saved in list_saved_searches(store)? {
        let Some(pin) = &saved.filters.pin else {
            continue;
        };
        if let Some(stale) = check_pinned(store, &saved, pin, now)? {
            out.push(stale);
        }
    }
    Ok(out)
}

/// `saved`'s staleness, `None` when its answer is current.
fn check_pinned(
    store: &Store<'_>,
    saved: &SavedSearch,
    pin: &PinnedAnswer,
    now: DateTime<Utc>,
) -> Result<Option<StalePinned>> {
    let mut stale = StalePinned {
        name: saved.name.clone(),
        answer_slug: pin.answer_slug.clone(),
        reasons: Vec::new(),
        new_evidence: Vec::new(),
        evidence_gone: Vec::new(),
    };
    let (Some(slug), Some(answered_at)) = (&pin.answer_slug, &pin.answered_at) else {
        stale.reasons.push(StaleReason::Unanswered);
        return Ok(Some(stale));
    };
    if pin.answered_query.as_deref() != Some(saved.query.as_str()) {
        stale.reasons.push(StaleReason::QueryChanged);
    }
    if crate::wiki::get_wiki_page(store, slug)?.is_none() {
        stale.reasons.push(StaleReason::AnswerMissing);
    }
    let live = live_ids(store, &pin.evidence_ids)?;
    stale.evidence_gone = pin
        .evidence_ids
        .iter()
        .filter(|id| !live.contains(*id))
        .cloned()
        .collect();
    if !stale.evidence_gone.is_empty() {
        stale.reasons.push(StaleReason::EvidenceGone);
    }

    let answered = parse_time(answered_at);
    let new_results = new_results(store, saved, pin, answered)?;
    if !new_results.is_empty() {
        stale.reasons.push(StaleReason::NewResults);
    }
    let related = stale_check(store, &pin.evidence_ids, answered_at, now)?;
    if related.is_stale() {
        stale.reasons.push(StaleReason::RelatedEvidence);
    }
    stale.new_evidence = dedup(
        &new_results
            .into_iter()
            .chain(related.new_evidence)
            .collect::<Vec<_>>(),
    );
    stale.new_evidence.truncate(NEW_EVIDENCE_MAX);

    Ok((!stale.reasons.is_empty()).then_some(stale))
}

/// Results of `saved` outside the answer's evidence that were written or
/// changed after the answer, newest first.
///
/// Membership alone would flag questions when nothing new arrived: ranking
/// moves with vitality and access counts, so an older memory can enter the
/// top results on its own.
fn new_results(
    store: &Store<'_>,
    saved: &SavedSearch,
    pin: &PinnedAnswer,
    answered: Option<DateTime<Utc>>,
) -> Result<Vec<String>> {
    let results = search_memories(store, &build_search_input(saved, Some(POLL_RESULT_LIMIT)))?;
    let mut fresh: Vec<(DateTime<Utc>, String)> = results
        .into_iter()
        .map(|r| r.memory)
        .filter(|m| !pin.evidence_ids.contains(&m.id))
        .filter_map(|m| {
            let touched = [&m.created_at, &m.updated_at]
                .into_iter()
                .filter_map(|t| parse_time(t))
                .max()?;
            let after = answered.is_none_or(|a| touched > a);
            after.then_some((touched, m.id))
        })
        .collect();
    fresh.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(fresh.into_iter().map(|(_, id)| id).collect())
}

/// The ids among `ids` that name live, unsuperseded memories.
fn live_ids(store: &Store<'_>, ids: &[String]) -> Result<Vec<String>> {
    Ok(crate::promotion::load_memories(store, ids)?
        .into_iter()
        .filter(|m| m.deleted_at.is_none() && m.superseded_by.is_none())
        .map(|m| m.id)
        .collect())
}

fn dedup(ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(ids.len());
    for id in ids {
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn invalid(message: String) -> crate::db::StoreError {
    crate::db::StoreError::Invalid(message)
}
