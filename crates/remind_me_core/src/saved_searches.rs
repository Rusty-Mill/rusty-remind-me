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
use crate::models::{
    MemorySearchInput, SaveSearchInput, SavedSearch, SavedSearchFilters, POLL_RESULT_LIMIT,
};
use chrono::Utc;
use rusqlite::{Connection, Result};

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
pub fn save_search(conn: &Connection, input: &SaveSearchInput) -> Result<SavedSearch> {
    let repo = SavedSearches::new(conn);
    let now = Utc::now().to_rfc3339();
    let existing = repo.id_for_name(&input.name)?;
    let saved = SavedSearch {
        id: existing.clone().unwrap_or_else(|| make_id(&input.name)),
        name: input.name.clone(),
        query: input.query.clone(),
        filters: SavedSearchFilters {
            category: input.category.clone(),
            tags: input.tags.clone(),
            include_sensitive: input.include_sensitive,
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
pub fn list_saved_searches(conn: &Connection) -> Result<Vec<SavedSearch>> {
    SavedSearches::new(conn).list()
}

/// One saved search by name, or `None`.
pub fn get_saved_search(conn: &Connection, name: &str) -> Result<Option<SavedSearch>> {
    SavedSearches::new(conn).get_by_name(name)
}

/// Delete a saved search and its seen-memory rows. `false` if no saved
/// search has that name.
pub fn delete_saved_search(conn: &Connection, name: &str) -> Result<bool> {
    let repo = SavedSearches::new(conn);
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
    conn: &Connection,
    saved: &SavedSearch,
) -> Result<Vec<crate::models::MemorySearchResult>> {
    search_memories(conn, &build_search_input(saved, None))
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
pub fn poll_saved_search(conn: &Connection, saved: &SavedSearch) -> Result<PollOutcome> {
    let results = search_memories(conn, &build_search_input(saved, Some(POLL_RESULT_LIMIT)))?;
    let current: Vec<String> = results.into_iter().map(|r| r.memory.id).collect();
    let repo = SavedSearches::new(conn);
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
pub fn poll_watched_searches(conn: &Connection) -> Result<Vec<(String, PollOutcome)>> {
    let watched: Vec<SavedSearch> = list_saved_searches(conn)?
        .into_iter()
        .filter(|s| s.watch)
        .collect();

    let mut outcomes = Vec::with_capacity(watched.len());
    for saved in watched {
        let outcome = poll_saved_search(conn, &saved)?;
        outcomes.push((saved.name, outcome));
    }
    Ok(outcomes)
}
