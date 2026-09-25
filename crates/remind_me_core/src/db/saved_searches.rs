//! Storage for saved searches: the `saved_searches` table and the
//! `saved_search_seen_memories` rows that record what a watch has reported.
//!
//! Every statement against those two tables lives here (ADR-0022). The
//! rules for what a saved search means (update by name, seeding a first
//! poll, which matches are new) stay in [`crate::saved_searches`], which
//! calls this rather than writing SQL.

use crate::models::{SavedSearch, SavedSearchFilters};
use rusqlite::{params, Connection, OptionalExtension, Result};
use std::collections::HashSet;

const SELECT_COLUMNS: &str = "id, name, query, filters, watch, created_at, updated_at";

/// The saved-search tables, over one connection.
pub struct SavedSearches<'c> {
    conn: &'c Connection,
}

impl<'c> SavedSearches<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// The id of the saved search called `name`, if there is one.
    pub fn id_for_name(&self, name: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT id FROM saved_searches WHERE name = ?",
                params![name],
                |r| r.get(0),
            )
            .optional()
    }

    /// Store a new saved search, every field as given.
    pub fn insert(&self, saved: &SavedSearch) -> Result<()> {
        self.conn.execute(
            "INSERT INTO saved_searches
                 (id, name, query, filters, watch, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                saved.id,
                saved.name,
                saved.query,
                encode_filters(&saved.filters),
                saved.watch as i64,
                saved.created_at,
                saved.updated_at
            ],
        )?;
        Ok(())
    }

    /// Overwrite the query, filters, watch flag and `updated_at` of the saved
    /// search with `saved.id`. Its name and `created_at` are left as stored.
    pub fn update(&self, saved: &SavedSearch) -> Result<()> {
        self.conn.execute(
            "UPDATE saved_searches
                SET query = ?, filters = ?, watch = ?, updated_at = ?
              WHERE id = ?",
            params![
                saved.query,
                encode_filters(&saved.filters),
                saved.watch as i64,
                saved.updated_at,
                saved.id
            ],
        )?;
        Ok(())
    }

    /// One saved search by id. Errors with `QueryReturnedNoRows` if absent.
    pub fn get(&self, id: &str) -> Result<SavedSearch> {
        self.conn.query_row(
            &format!("SELECT {SELECT_COLUMNS} FROM saved_searches WHERE id = ?"),
            params![id],
            read_row,
        )
    }

    /// One saved search by name, or `None`.
    pub fn get_by_name(&self, name: &str) -> Result<Option<SavedSearch>> {
        self.conn
            .query_row(
                &format!("SELECT {SELECT_COLUMNS} FROM saved_searches WHERE name = ?"),
                params![name],
                read_row,
            )
            .optional()
    }

    /// Every saved search, alphabetical by name.
    pub fn list(&self) -> Result<Vec<SavedSearch>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SELECT_COLUMNS} FROM saved_searches ORDER BY name ASC"
        ))?;
        let rows = stmt.query_map([], read_row)?.collect();
        rows
    }

    /// Delete the saved search with `id` and its seen-memory rows.
    ///
    /// The seen rows go explicitly rather than being left keyed by an id that
    /// no longer resolves: nothing will ever query them again.
    pub fn delete(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM saved_search_seen_memories WHERE saved_search_id = ?",
            params![id],
        )?;
        self.conn
            .execute("DELETE FROM saved_searches WHERE id = ?", params![id])?;
        Ok(())
    }

    /// Every memory id a watch on `saved_search_id` has recorded as seen.
    pub fn seen_ids(&self, saved_search_id: &str) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT memory_id FROM saved_search_seen_memories WHERE saved_search_id = ?",
        )?;
        let ids = stmt
            .query_map(params![saved_search_id], |r| r.get::<_, String>(0))?
            .collect();
        ids
    }

    /// Whether a watch on `saved_search_id` has recorded anything yet.
    pub fn has_any_seen(&self, saved_search_id: &str) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM saved_search_seen_memories WHERE saved_search_id = ? LIMIT 1",
                params![saved_search_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Record `memory_ids` as seen by `saved_search_id` at `now`. An id
    /// already recorded keeps its first `first_seen_at`.
    pub fn mark_seen(&self, saved_search_id: &str, memory_ids: &[String], now: &str) -> Result<()> {
        for memory_id in memory_ids {
            // OR IGNORE rather than a pre-check: a memory that matched on two
            // consecutive polls is the common case, not an error.
            self.conn.execute(
                "INSERT OR IGNORE INTO saved_search_seen_memories
                     (saved_search_id, memory_id, first_seen_at)
                 VALUES (?, ?, ?)",
                params![saved_search_id, memory_id, now],
            )?;
        }
        Ok(())
    }
}

/// The `filters` column's JSON. Serialising three plain fields cannot fail;
/// `{}` is the empty filter set if it somehow did.
fn encode_filters(filters: &SavedSearchFilters) -> String {
    serde_json::to_string(filters).unwrap_or_else(|_| "{}".to_string())
}

fn read_row(row: &rusqlite::Row<'_>) -> Result<SavedSearch> {
    let filters_json: String = row.get(3)?;
    let watch: i64 = row.get(4)?;
    Ok(SavedSearch {
        id: row.get(0)?,
        name: row.get(1)?,
        query: row.get(2)?,
        // Malformed filters read as empty rather than as an error: the saved
        // search still has a usable query, and refusing to list it would hide
        // the one thing a caller needs in order to fix or delete it.
        filters: serde_json::from_str(&filters_json).unwrap_or_default(),
        watch: watch != 0,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn saved(id: &str, name: &str) -> SavedSearch {
        SavedSearch {
            id: id.to_string(),
            name: name.to_string(),
            query: "q".to_string(),
            filters: SavedSearchFilters::default(),
            watch: false,
            created_at: "2026-09-25T00:00:00+00:00".to_string(),
            updated_at: "2026-09-25T00:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn update_keeps_the_stored_name_and_created_at() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let repo = SavedSearches::new(&conn);
        repo.insert(&saved("ss_1", "one")).unwrap();

        let mut changed = saved("ss_1", "renamed");
        changed.query = "new query".to_string();
        changed.watch = true;
        changed.created_at = "2030-01-01T00:00:00+00:00".to_string();
        changed.updated_at = "2026-09-26T00:00:00+00:00".to_string();
        repo.update(&changed).unwrap();

        let stored = repo.get("ss_1").unwrap();
        assert_eq!(stored.name, "one");
        assert_eq!(stored.created_at, "2026-09-25T00:00:00+00:00");
        assert_eq!(stored.query, "new query");
        assert!(stored.watch);
        assert_eq!(stored.updated_at, "2026-09-26T00:00:00+00:00");
    }

    #[test]
    fn malformed_filters_read_as_empty() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let repo = SavedSearches::new(&conn);
        repo.insert(&saved("ss_1", "one")).unwrap();
        conn.execute(
            "UPDATE saved_searches SET filters = 'not json' WHERE id = 'ss_1'",
            [],
        )
        .unwrap();
        assert_eq!(
            repo.get_by_name("one").unwrap().unwrap().filters,
            SavedSearchFilters::default()
        );
    }

    #[test]
    fn mark_seen_keeps_the_first_sighting() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let repo = SavedSearches::new(&conn);
        assert!(!repo.has_any_seen("ss_1").unwrap());
        repo.mark_seen("ss_1", &["m1".to_string()], "2026-09-25T00:00:00+00:00")
            .unwrap();
        repo.mark_seen(
            "ss_1",
            &["m1".to_string(), "m2".to_string()],
            "2026-09-26T00:00:00+00:00",
        )
        .unwrap();
        assert!(repo.has_any_seen("ss_1").unwrap());
        assert_eq!(
            repo.seen_ids("ss_1").unwrap(),
            HashSet::from(["m1".to_string(), "m2".to_string()])
        );
        let first: String = conn
            .query_row(
                "SELECT first_seen_at FROM saved_search_seen_memories WHERE memory_id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(first, "2026-09-25T00:00:00+00:00");
    }
}
