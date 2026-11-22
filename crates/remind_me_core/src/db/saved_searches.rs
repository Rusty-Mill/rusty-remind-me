//! Storage for saved searches: the `saved_searches` table and the
//! `saved_search_seen_memories` rows that record what a watch has reported.
//!
//! [`SavedSearchStore`] is the port: the one interface a backend implements
//! for this table group (ADR-0035). The engine's adapter is in
//! `db::engine::saved_searches`. [`SavedSearches`] is what callers take: it
//! locks the store's tables for each call and goes through the port.
//!
//! Every read and write against those two tables goes through here
//! (ADR-0022). The rules for what a saved search means (update by name,
//! seeding a first poll, which matches are new) stay in
//! [`crate::saved_searches`].

use super::backend::BackendLock;
use super::{Result, Store};
use crate::models::{SavedSearch, SavedSearchFilters};
use std::collections::HashSet;

/// The saved-search tables, as a backend stores them (ADR-0035).
///
/// Ids are the caller's own strings; a backend keys rows however it likes,
/// but reads and writes by id must behave as a lookup on that exact string.
/// [`conformance`] holds the behaviour every implementation must show.
pub trait SavedSearchStore {
    /// The id of the saved search called `name`, if there is one.
    fn id_for_name(&self, name: &str) -> Result<Option<String>>;

    /// Store a new saved search, every field as given. A name already in
    /// use is refused with [`super::StoreError::Invalid`].
    fn insert(&mut self, saved: &SavedSearch) -> Result<()>;

    /// Overwrite the query, filters, watch flag and `updated_at` of the saved
    /// search with `saved.id`, keeping its name and `created_at`. A missing
    /// id changes nothing.
    fn update(&mut self, saved: &SavedSearch) -> Result<()>;

    /// One saved search by id. [`super::StoreError::NotFound`] if absent.
    fn get(&self, id: &str) -> Result<SavedSearch>;

    /// One saved search by name, or `None`.
    fn get_by_name(&self, name: &str) -> Result<Option<SavedSearch>>;

    /// Every saved search, by name in byte order.
    fn list(&self) -> Result<Vec<SavedSearch>>;

    /// Delete the saved search with `id` and its seen-memory rows. A missing
    /// id is not an error.
    fn delete(&mut self, id: &str) -> Result<()>;

    /// Every memory id a watch on `saved_search_id` has recorded as seen.
    fn seen_ids(&self, saved_search_id: &str) -> Result<HashSet<String>>;

    /// Whether a watch on `saved_search_id` has recorded anything yet.
    fn has_any_seen(&self, saved_search_id: &str) -> Result<bool>;

    /// Record `memory_ids` as seen by `saved_search_id` at `now`. An id
    /// already recorded keeps its first sighting.
    fn mark_seen(&mut self, saved_search_id: &str, memory_ids: &[String], now: &str) -> Result<()>;
}

/// The saved-search tables of a [`Store`], one locked call at a time.
pub struct SavedSearches<'c> {
    backend: &'c BackendLock,
}

impl<'c> SavedSearches<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            backend: store.backend(),
        }
    }

    /// See [`SavedSearchStore::id_for_name`].
    pub fn id_for_name(&self, name: &str) -> Result<Option<String>> {
        SavedSearchStore::id_for_name(&*self.backend.lock()?, name)
    }

    /// See [`SavedSearchStore::insert`].
    pub fn insert(&self, saved: &SavedSearch) -> Result<()> {
        SavedSearchStore::insert(&mut *self.backend.lock()?, saved)
    }

    /// See [`SavedSearchStore::update`].
    pub fn update(&self, saved: &SavedSearch) -> Result<()> {
        SavedSearchStore::update(&mut *self.backend.lock()?, saved)
    }

    /// See [`SavedSearchStore::get`].
    pub fn get(&self, id: &str) -> Result<SavedSearch> {
        SavedSearchStore::get(&*self.backend.lock()?, id)
    }

    /// See [`SavedSearchStore::get_by_name`].
    pub fn get_by_name(&self, name: &str) -> Result<Option<SavedSearch>> {
        SavedSearchStore::get_by_name(&*self.backend.lock()?, name)
    }

    /// See [`SavedSearchStore::list`].
    pub fn list(&self) -> Result<Vec<SavedSearch>> {
        SavedSearchStore::list(&*self.backend.lock()?)
    }

    /// See [`SavedSearchStore::delete`].
    pub fn delete(&self, id: &str) -> Result<()> {
        SavedSearchStore::delete(&mut *self.backend.lock()?, id)
    }

    /// See [`SavedSearchStore::seen_ids`].
    pub fn seen_ids(&self, saved_search_id: &str) -> Result<HashSet<String>> {
        SavedSearchStore::seen_ids(&*self.backend.lock()?, saved_search_id)
    }

    /// See [`SavedSearchStore::has_any_seen`].
    pub fn has_any_seen(&self, saved_search_id: &str) -> Result<bool> {
        SavedSearchStore::has_any_seen(&*self.backend.lock()?, saved_search_id)
    }

    /// See [`SavedSearchStore::mark_seen`].
    pub fn mark_seen(&self, saved_search_id: &str, memory_ids: &[String], now: &str) -> Result<()> {
        SavedSearchStore::mark_seen(&mut *self.backend.lock()?, saved_search_id, memory_ids, now)
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

/// The behaviour every [`SavedSearchStore`] must show, written once and run
/// against each backend by [`saved_search_conformance`] (ADR-0035).
#[doc(hidden)]
pub mod conformance {
    use super::SavedSearchStore;
    use crate::db::StoreError;
    use crate::models::{SavedSearch, SavedSearchFilters};
    use std::collections::HashSet;

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

    pub fn round_trips_every_field(store: &mut dyn SavedSearchStore) {
        let mut one = saved("ss_1", "one");
        one.filters.category = Some("decision".to_string());
        one.filters.tags = Some(vec!["t".to_string()]);
        one.watch = true;
        store.insert(&one).unwrap();
        assert_eq!(store.get("ss_1").unwrap(), one);
        assert_eq!(store.get_by_name("one").unwrap(), Some(one));
    }

    pub fn update_keeps_the_stored_name_and_created_at(store: &mut dyn SavedSearchStore) {
        store.insert(&saved("ss_1", "one")).unwrap();

        let mut changed = saved("ss_1", "renamed");
        changed.query = "new query".to_string();
        changed.watch = true;
        changed.created_at = "2030-01-01T00:00:00+00:00".to_string();
        changed.updated_at = "2026-09-26T00:00:00+00:00".to_string();
        store.update(&changed).unwrap();

        let stored = store.get("ss_1").unwrap();
        assert_eq!(stored.name, "one");
        assert_eq!(stored.created_at, "2026-09-25T00:00:00+00:00");
        assert_eq!(stored.query, "new query");
        assert!(stored.watch);
        assert_eq!(stored.updated_at, "2026-09-26T00:00:00+00:00");
    }

    pub fn updating_a_missing_id_changes_nothing(store: &mut dyn SavedSearchStore) {
        store.update(&saved("ss_missing", "ghost")).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    pub fn a_name_in_use_is_refused(store: &mut dyn SavedSearchStore) {
        store.insert(&saved("ss_1", "one")).unwrap();
        assert!(matches!(
            store.insert(&saved("ss_2", "one")),
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(store.id_for_name("one").unwrap().as_deref(), Some("ss_1"));
    }

    pub fn a_missing_id_is_not_found(store: &mut dyn SavedSearchStore) {
        assert!(matches!(store.get("nope"), Err(StoreError::NotFound)));
        assert!(store.get_by_name("nope").unwrap().is_none());
        assert!(store.id_for_name("nope").unwrap().is_none());
    }

    pub fn list_orders_names_by_byte_value(store: &mut dyn SavedSearchStore) {
        for (id, name) in [("1", "beta"), ("2", "Alpha"), ("3", "alpha")] {
            store.insert(&saved(id, name)).unwrap();
        }
        let names: Vec<String> = store.list().unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["Alpha", "alpha", "beta"]);
    }

    pub fn delete_takes_the_seen_rows_with_it(store: &mut dyn SavedSearchStore) {
        store.insert(&saved("ss_1", "one")).unwrap();
        store
            .mark_seen("ss_1", &["m1".to_string()], "2026-09-25T00:00:00+00:00")
            .unwrap();
        store.delete("ss_1").unwrap();
        assert!(store.id_for_name("one").unwrap().is_none());
        assert!(!store.has_any_seen("ss_1").unwrap());
        // Deleting again is not an error, as a delete of no rows is not.
        store.delete("ss_1").unwrap();
    }

    pub fn mark_seen_keeps_the_first_sighting(store: &mut dyn SavedSearchStore) {
        assert!(!store.has_any_seen("ss_1").unwrap());
        store
            .mark_seen("ss_1", &["m1".to_string()], "2026-09-25T00:00:00+00:00")
            .unwrap();
        store
            .mark_seen(
                "ss_1",
                &["m1".to_string(), "m2".to_string()],
                "2026-09-26T00:00:00+00:00",
            )
            .unwrap();
        assert!(store.has_any_seen("ss_1").unwrap());
        assert_eq!(
            store.seen_ids("ss_1").unwrap(),
            HashSet::from(["m1".to_string(), "m2".to_string()])
        );
        assert!(!store.has_any_seen("ss_2").unwrap());
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`. A backend's adapter invokes it in its own test module.
#[doc(hidden)]
#[macro_export]
macro_rules! saved_search_conformance {
    ($make:expr) => {
        use $crate::db::saved_searches::conformance;

        #[test]
        fn round_trips_every_field() {
            conformance::round_trips_every_field(&mut $make);
        }
        #[test]
        fn update_keeps_the_stored_name_and_created_at() {
            conformance::update_keeps_the_stored_name_and_created_at(&mut $make);
        }
        #[test]
        fn updating_a_missing_id_changes_nothing() {
            conformance::updating_a_missing_id_changes_nothing(&mut $make);
        }
        #[test]
        fn a_name_in_use_is_refused() {
            conformance::a_name_in_use_is_refused(&mut $make);
        }
        #[test]
        fn a_missing_id_is_not_found() {
            conformance::a_missing_id_is_not_found(&mut $make);
        }
        #[test]
        fn list_orders_names_by_byte_value() {
            conformance::list_orders_names_by_byte_value(&mut $make);
        }
        #[test]
        fn delete_takes_the_seen_rows_with_it() {
            conformance::delete_takes_the_seen_rows_with_it(&mut $make);
        }
        #[test]
        fn mark_seen_keeps_the_first_sighting() {
            conformance::mark_seen_keeps_the_first_sighting(&mut $make);
        }
    };
}
#[doc(hidden)]
pub use crate::saved_search_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn the_repository_reaches_the_store_through_the_port() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let repo = SavedSearches::new(&store);
        let saved = SavedSearch {
            id: "ss_1".to_string(),
            name: "one".to_string(),
            query: "q".to_string(),
            filters: SavedSearchFilters::default(),
            watch: true,
            created_at: "2026-09-25T00:00:00+00:00".to_string(),
            updated_at: "2026-09-25T00:00:00+00:00".to_string(),
        };
        repo.insert(&saved).unwrap();
        repo.mark_seen("ss_1", &["m1".to_string()], "2026-09-25T00:00:00+00:00")
            .unwrap();
        assert_eq!(repo.list().unwrap(), [saved]);
        assert!(repo.has_any_seen("ss_1").unwrap());
        repo.delete("ss_1").unwrap();
        assert!(repo.get_by_name("one").unwrap().is_none());
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
}
