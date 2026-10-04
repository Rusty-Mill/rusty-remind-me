//! Storage for saved searches: the `saved_searches` table and the
//! `saved_search_seen_memories` rows that record what a watch has reported,
//! on the engine (`db::engine::saved_searches`).
//!
//! Every read and write against those two tables goes through here
//! (ADR-0022). The rules for what a saved search means (update by name,
//! seeding a first poll, which matches are new) stay in
//! [`crate::saved_searches`].

use super::engine::{self, EngineLock};
use super::{Result, Store};
use crate::models::{SavedSearch, SavedSearchFilters};
use std::collections::HashSet;

/// The saved-search tables, on the engine.
pub struct SavedSearches<'c> {
    engine: &'c EngineLock,
}

impl<'c> SavedSearches<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            engine: store.engine(),
        }
    }

    /// The id of the saved search called `name`, if there is one.
    pub fn id_for_name(&self, name: &str) -> Result<Option<String>> {
        Ok(engine::saved_searches::id_for_name(
            &self.engine.lock(),
            name,
        ))
    }

    /// Store a new saved search, every field as given.
    pub fn insert(&self, saved: &SavedSearch) -> Result<()> {
        engine::saved_searches::insert(&mut self.engine.lock(), saved)
    }

    /// Overwrite the query, filters, watch flag and `updated_at` of the saved
    /// search with `saved.id`. Its name and `created_at` are left as stored.
    pub fn update(&self, saved: &SavedSearch) -> Result<()> {
        engine::saved_searches::update(&mut self.engine.lock(), saved)
    }

    /// One saved search by id. [`super::StoreError::NotFound`] if absent.
    pub fn get(&self, id: &str) -> Result<SavedSearch> {
        engine::saved_searches::get(&self.engine.lock(), id)
    }

    /// One saved search by name, or `None`.
    pub fn get_by_name(&self, name: &str) -> Result<Option<SavedSearch>> {
        Ok(engine::saved_searches::get_by_name(
            &self.engine.lock(),
            name,
        ))
    }

    /// Every saved search, alphabetical by name.
    pub fn list(&self) -> Result<Vec<SavedSearch>> {
        Ok(engine::saved_searches::list(&self.engine.lock()))
    }

    /// Delete the saved search with `id` and its seen-memory rows.
    ///
    /// The seen rows go explicitly rather than being left keyed by an id that
    /// no longer resolves: nothing will ever query them again.
    pub fn delete(&self, id: &str) -> Result<()> {
        engine::saved_searches::delete(&mut self.engine.lock(), id)
    }

    /// Every memory id a watch on `saved_search_id` has recorded as seen.
    pub fn seen_ids(&self, saved_search_id: &str) -> Result<HashSet<String>> {
        Ok(engine::saved_searches::seen_ids(
            &self.engine.lock(),
            saved_search_id,
        ))
    }

    /// Whether a watch on `saved_search_id` has recorded anything yet.
    pub fn has_any_seen(&self, saved_search_id: &str) -> Result<bool> {
        Ok(engine::saved_searches::has_any_seen(
            &self.engine.lock(),
            saved_search_id,
        ))
    }

    /// Record `memory_ids` as seen by `saved_search_id` at `now`. An id
    /// already recorded keeps its first `first_seen_at`.
    pub fn mark_seen(&self, saved_search_id: &str, memory_ids: &[String], now: &str) -> Result<()> {
        engine::saved_searches::mark_seen(&mut self.engine.lock(), saved_search_id, memory_ids, now)
    }
}

/// The `filters` column's JSON. Serialising three plain fields cannot fail;
/// `{}` is the empty filter set if it somehow did.
pub(crate) fn encode_filters(filters: &SavedSearchFilters) -> String {
    serde_json::to_string(filters).unwrap_or_else(|_| "{}".to_string())
}

/// The filters stored as `json`. Malformed filters read as empty rather than
/// as an error: the saved search still has a usable query, and refusing to
/// list it would hide the one thing a caller needs in order to fix or
/// delete it.
pub(crate) fn decode_filters(json: &str) -> SavedSearchFilters {
    serde_json::from_str(json).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, StoreError};

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

    fn repo_on(db: &Database, test: impl FnOnce(&SavedSearches<'_>)) {
        let store = db.store();
        test(&SavedSearches::new(&store));
    }

    #[test]
    fn update_keeps_the_stored_name_and_created_at() {
        repo_on(&Database::open_in_memory().unwrap(), |repo| {
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
        });
    }

    #[test]
    fn updating_a_missing_id_changes_nothing() {
        repo_on(&Database::open_in_memory().unwrap(), |repo| {
            repo.update(&saved("ss_missing", "ghost")).unwrap();
            assert!(repo.list().unwrap().is_empty());
        });
    }

    #[test]
    fn a_name_in_use_is_refused() {
        repo_on(&Database::open_in_memory().unwrap(), |repo| {
            repo.insert(&saved("ss_1", "one")).unwrap();
            assert!(repo.insert(&saved("ss_2", "one")).is_err());
            assert_eq!(repo.id_for_name("one").unwrap().as_deref(), Some("ss_1"));
        });
    }

    #[test]
    fn a_missing_id_is_not_found() {
        repo_on(&Database::open_in_memory().unwrap(), |repo| {
            assert!(matches!(repo.get("nope"), Err(StoreError::NotFound)));
            assert!(repo.get_by_name("nope").unwrap().is_none());
            assert!(repo.id_for_name("nope").unwrap().is_none());
        });
    }

    #[test]
    fn list_orders_names_by_byte_value() {
        repo_on(&Database::open_in_memory().unwrap(), |repo| {
            for (id, name) in [("1", "beta"), ("2", "Alpha"), ("3", "alpha")] {
                repo.insert(&saved(id, name)).unwrap();
            }
            let names: Vec<String> = repo.list().unwrap().into_iter().map(|s| s.name).collect();
            assert_eq!(names, ["Alpha", "alpha", "beta"]);
        });
    }

    #[test]
    fn delete_takes_the_seen_rows_with_it() {
        repo_on(&Database::open_in_memory().unwrap(), |repo| {
            repo.insert(&saved("ss_1", "one")).unwrap();
            repo.mark_seen("ss_1", &["m1".to_string()], "2026-09-25T00:00:00+00:00")
                .unwrap();
            repo.delete("ss_1").unwrap();
            assert!(repo.id_for_name("one").unwrap().is_none());
            assert!(!repo.has_any_seen("ss_1").unwrap());
            // Deleting again is not an error, as a delete of no rows is not.
            repo.delete("ss_1").unwrap();
        });
    }

    #[test]
    fn malformed_filters_read_as_empty() {
        assert_eq!(decode_filters("not json"), SavedSearchFilters::default());
        assert_eq!(
            encode_filters(&SavedSearchFilters::default())
                .chars()
                .next(),
            Some('{')
        );
    }

    #[test]
    fn mark_seen_keeps_the_first_sighting() {
        repo_on(&Database::open_in_memory().unwrap(), |repo| {
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
        });
    }
}
