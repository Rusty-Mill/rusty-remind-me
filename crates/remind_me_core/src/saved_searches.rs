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
use crate::models::{
    MemorySearchInput, SaveSearchInput, SavedSearch, SavedSearchFilters, POLL_RESULT_LIMIT,
};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Result};

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

fn row_to_saved_search(
    id: String,
    name: String,
    query: String,
    filters_json: String,
    watch: i64,
    created_at: String,
    updated_at: String,
) -> SavedSearch {
    // Malformed filters are treated as empty rather than as an error: the
    // saved search still has a usable query, and refusing to list it would
    // hide the one thing a caller needs in order to fix or delete it.
    let filters = serde_json::from_str::<SavedSearchFilters>(&filters_json).unwrap_or_default();
    SavedSearch {
        id,
        name,
        query,
        filters,
        watch: watch != 0,
        created_at,
        updated_at,
    }
}

const SELECT_COLUMNS: &str = "id, name, query, filters, watch, created_at, updated_at";

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SavedSearch> {
    Ok(row_to_saved_search(
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

/// Create a saved search, or update it in place when the name already exists.
///
/// Update-by-name rather than a duplicate insert: re-saving under the same
/// name is how a caller changes a saved search's query, filters or watch flag.
/// The same "one name is one logical thing" convention `remind_me_wiki_write`
/// already uses for pages, and what the table's `UNIQUE` on `name` implies.
pub fn save_search(conn: &Connection, input: &SaveSearchInput) -> Result<SavedSearch> {
    let filters = SavedSearchFilters {
        category: input.category.clone(),
        tags: input.tags.clone(),
        include_sensitive: input.include_sensitive,
    };
    let filters_json = serde_json::to_string(&filters).unwrap_or_else(|_| "{}".to_string());
    let now = Utc::now().to_rfc3339();

    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM saved_searches WHERE name = ?",
            params![input.name],
            |r| r.get(0),
        )
        .optional()?;

    let id = match existing {
        Some(id) => {
            conn.execute(
                "UPDATE saved_searches
                    SET query = ?, filters = ?, watch = ?, updated_at = ?
                  WHERE id = ?",
                params![input.query, filters_json, input.watch as i64, now, id],
            )?;
            id
        }
        None => {
            let id = make_id(&input.name);
            conn.execute(
                "INSERT INTO saved_searches
                     (id, name, query, filters, watch, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    id,
                    input.name,
                    input.query,
                    filters_json,
                    input.watch as i64,
                    now,
                    now
                ],
            )?;
            id
        }
    };

    conn.query_row(
        &format!("SELECT {} FROM saved_searches WHERE id = ?", SELECT_COLUMNS),
        params![id],
        read_row,
    )
}

/// Every saved search, alphabetical by name.
pub fn list_saved_searches(conn: &Connection) -> Result<Vec<SavedSearch>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM saved_searches ORDER BY name ASC",
        SELECT_COLUMNS
    ))?;
    let rows = stmt.query_map([], read_row)?.collect();
    rows
}

/// One saved search by name, or `None`.
pub fn get_saved_search(conn: &Connection, name: &str) -> Result<Option<SavedSearch>> {
    conn.query_row(
        &format!(
            "SELECT {} FROM saved_searches WHERE name = ?",
            SELECT_COLUMNS
        ),
        params![name],
        read_row,
    )
    .optional()
}

/// Delete a saved search and its seen-memory rows.
///
/// The tracking rows go explicitly rather than being left keyed by an id that
/// no longer resolves: nothing will ever query them again, so they are dead
/// weight from the moment the parent row goes. Same discipline as
/// `delete_memory`'s chunk-vector cleanup.
pub fn delete_saved_search(conn: &Connection, name: &str) -> Result<bool> {
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM saved_searches WHERE name = ?",
            params![name],
            |r| r.get(0),
        )
        .optional()?;

    let Some(id) = id else {
        return Ok(false);
    };

    conn.execute(
        "DELETE FROM saved_search_seen_memories WHERE saved_search_id = ?",
        params![id],
    )?;
    conn.execute("DELETE FROM saved_searches WHERE id = ?", params![id])?;
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

fn seen_ids(conn: &Connection, saved_search_id: &str) -> Result<std::collections::HashSet<String>> {
    let mut stmt =
        conn.prepare("SELECT memory_id FROM saved_search_seen_memories WHERE saved_search_id = ?")?;
    let ids = stmt
        .query_map(params![saved_search_id], |r| r.get::<_, String>(0))?
        .collect();
    ids
}

fn has_any_seen(conn: &Connection, saved_search_id: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM saved_search_seen_memories WHERE saved_search_id = ? LIMIT 1",
            params![saved_search_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

fn mark_seen(conn: &Connection, saved_search_id: &str, memory_ids: &[String]) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    for memory_id in memory_ids {
        // OR IGNORE rather than a pre-check: a memory that matched on two
        // consecutive polls is the common case, not an error.
        conn.execute(
            "INSERT OR IGNORE INTO saved_search_seen_memories
                 (saved_search_id, memory_id, first_seen_at)
             VALUES (?, ?, ?)",
            params![saved_search_id, memory_id, now],
        )?;
    }
    Ok(())
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

    if !has_any_seen(conn, &saved.id)? {
        mark_seen(conn, &saved.id, &current)?;
        return Ok(PollOutcome {
            new_matches: Vec::new(),
            seeded: true,
        });
    }

    let already = seen_ids(conn, &saved.id)?;
    let new_matches: Vec<String> = current
        .iter()
        .filter(|id| !already.contains(*id))
        .cloned()
        .collect();

    mark_seen(conn, &saved.id, &current)?;
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
