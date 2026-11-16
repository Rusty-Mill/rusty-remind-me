//! Saved searches and their seen-memory rows on the engine: the first table
//! group moved (ADR-0023, phase 4b), and the engine's adapter for the
//! [`SavedSearchStore`] port (ADR-0035). The behaviour matches the old
//! SQLite statements one for one.

use super::{
    deleted, engine_error, engine_id, ensure_same_id, micros, pair_engine_id, EngineTables,
};
use crate::db::saved_searches::{decode_filters, encode_filters, SavedSearchStore};
use crate::db::{Result, StoreError};
use crate::models::SavedSearch;
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

/// Index marker: a saved search by its name, which is unique.
pub struct ByName;
/// Index marker: a seen row by the saved search that recorded it.
pub struct BySearch;
/// Slot marker: the watch flag, 0 or 1.
pub struct Watch;
/// Slot marker: when a memory was first seen, in µs since the epoch.
pub struct FirstSeen;

pub(crate) type SavedSearchTable = GenericMmapStore<SavedSearchRow, ByName, Watch>;
pub(crate) type SeenTable = GenericMmapStore<SeenRow, BySearch, FirstSeen>;

/// One `saved_searches` row. `filters` stays JSON text, as in SQLite: the
/// engine encodes records with bincode, which cannot carry the filters'
/// optional-field serde attributes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedSearchRow {
    engine_id: Uuid,
    id: String,
    name: String,
    query: String,
    filters: String,
    watch: i64,
    created_at: String,
    updated_at: String,
}

impl SavedSearchRow {
    fn new(saved: &SavedSearch) -> Self {
        Self {
            engine_id: engine_id(&saved.id),
            id: saved.id.clone(),
            name: saved.name.clone(),
            query: saved.query.clone(),
            filters: encode_filters(&saved.filters),
            watch: i64::from(saved.watch),
            created_at: saved.created_at.clone(),
            updated_at: saved.updated_at.clone(),
        }
    }

    fn to_model(&self) -> SavedSearch {
        SavedSearch {
            id: self.id.clone(),
            name: self.name.clone(),
            query: self.query.clone(),
            filters: decode_filters(&self.filters),
            watch: self.watch != 0,
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }
}

impl Record for SavedSearchRow {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for SavedSearchRow {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::SavedSearchRow@1";
}

impl IndexedField<ByName> for SavedSearchRow {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.name
    }
}

impl ScannableField<Watch> for SavedSearchRow {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.watch
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.watch = value;
    }
}

/// One `saved_search_seen_memories` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeenRow {
    engine_id: Uuid,
    saved_search_id: String,
    memory_id: String,
    first_seen_at: String,
    first_seen_us: i64,
}

impl Record for SeenRow {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for SeenRow {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::SeenRow@1";
}

impl IndexedField<BySearch> for SeenRow {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.saved_search_id
    }
}

impl ScannableField<FirstSeen> for SeenRow {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.first_seen_us
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.first_seen_us = value;
    }
}

impl SavedSearchStore for EngineTables {
    fn id_for_name(&self, name: &str) -> Result<Option<String>> {
        Ok(by_name(self, name).map(|row| row.id))
    }

    /// A name already in use is refused, as SQLite's `UNIQUE` constraint
    /// refuses it.
    fn insert(&mut self, saved: &SavedSearch) -> Result<()> {
        if by_name(self, &saved.name).is_some() {
            return Err(StoreError::Invalid(format!(
                "a saved search named {:?} already exists",
                saved.name
            )));
        }
        let row = SavedSearchRow::new(saved);
        if let Some(stored) = self.saved_searches.get(row.engine_id) {
            ensure_same_id(&stored.id, &saved.id)?;
        }
        self.saved_searches.insert(row).map_err(engine_error)
    }

    /// A missing id changes nothing, as an `UPDATE` matching no row does.
    fn update(&mut self, saved: &SavedSearch) -> Result<()> {
        let Some(stored) = self.saved_searches.get(engine_id(&saved.id)) else {
            return Ok(());
        };
        ensure_same_id(&stored.id, &saved.id)?;
        let row = SavedSearchRow {
            query: saved.query.clone(),
            filters: encode_filters(&saved.filters),
            watch: i64::from(saved.watch),
            updated_at: saved.updated_at.clone(),
            ..stored
        };
        self.saved_searches.replace(row).map_err(engine_error)
    }

    fn get(&self, id: &str) -> Result<SavedSearch> {
        let row = self
            .saved_searches
            .get(engine_id(id))
            .filter(|row| row.id == id)
            .ok_or(StoreError::NotFound)?;
        Ok(row.to_model())
    }

    fn get_by_name(&self, name: &str) -> Result<Option<SavedSearch>> {
        Ok(by_name(self, name).map(|row| row.to_model()))
    }

    /// By name in byte order: SQLite's `BINARY` collation.
    fn list(&self) -> Result<Vec<SavedSearch>> {
        let mut rows: Vec<SavedSearchRow> = self
            .saved_searches
            .all_ids()
            .into_iter()
            .filter_map(|id| self.saved_searches.get(id))
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(rows.iter().map(SavedSearchRow::to_model).collect())
    }

    /// A missing id is not an error, as a `DELETE` matching no row is not.
    fn delete(&mut self, id: &str) -> Result<()> {
        for seen in seen_rows(self, id) {
            deleted(self.seen.delete(seen))?;
        }
        let target = engine_id(id);
        match self.saved_searches.get(target) {
            Some(stored) if stored.id == id => deleted(self.saved_searches.delete(target)),
            _ => Ok(()),
        }
    }

    fn seen_ids(&self, saved_search_id: &str) -> Result<HashSet<String>> {
        Ok(seen_rows(self, saved_search_id)
            .into_iter()
            .filter_map(|id| self.seen.get(id))
            .map(|row| row.memory_id)
            .collect())
    }

    fn has_any_seen(&self, saved_search_id: &str) -> Result<bool> {
        Ok(!seen_rows(self, saved_search_id).is_empty())
    }

    /// An id already recorded keeps its first `first_seen_at`, as
    /// `INSERT OR IGNORE` does.
    fn mark_seen(&mut self, saved_search_id: &str, memory_ids: &[String], now: &str) -> Result<()> {
        let first_seen_us = micros(now);
        for memory_id in memory_ids {
            let key = pair_engine_id(saved_search_id, memory_id);
            if self.seen.get(key).is_some() {
                continue;
            }
            let row = SeenRow {
                engine_id: key,
                saved_search_id: saved_search_id.to_string(),
                memory_id: memory_id.clone(),
                first_seen_at: now.to_string(),
                first_seen_us,
            };
            self.seen.insert(row).map_err(engine_error)?;
        }
        Ok(())
    }
}

fn by_name(tables: &EngineTables, name: &str) -> Option<SavedSearchRow> {
    FilterEq::<SavedSearchRow, ByName>::filter_eq(&tables.saved_searches, &name.to_string())
        .into_iter()
        .find_map(|id| tables.saved_searches.get(id))
}

fn seen_rows(tables: &EngineTables, saved_search_id: &str) -> Vec<Uuid> {
    FilterEq::<SeenRow, BySearch>::filter_eq(&tables.seen, &saved_search_id.to_string())
}

/// The port's shared behaviour, on fresh temporary engine tables.
#[cfg(test)]
mod conformance_tests {
    use super::EngineTables;
    crate::db::saved_searches::saved_search_conformance!(EngineTables::open_temporary().unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SavedSearchFilters;

    fn saved(id: &str, name: &str) -> SavedSearch {
        SavedSearch {
            id: id.to_string(),
            name: name.to_string(),
            query: "q".to_string(),
            filters: SavedSearchFilters::default(),
            watch: true,
            created_at: "2026-09-25T00:00:00+00:00".to_string(),
            updated_at: "2026-09-25T00:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn the_first_seen_time_is_kept() {
        let mut tables = EngineTables::open_temporary().unwrap();
        tables
            .mark_seen("ss_1", &["m1".to_string()], "2026-09-25T00:00:00+00:00")
            .unwrap();
        tables
            .mark_seen("ss_1", &["m1".to_string()], "2026-09-26T00:00:00+00:00")
            .unwrap();
        let row = tables.seen.get(pair_engine_id("ss_1", "m1")).unwrap();
        assert_eq!(row.first_seen_at, "2026-09-25T00:00:00+00:00");
        assert_eq!(row.first_seen_us, micros("2026-09-25T00:00:00+00:00"));
    }

    #[test]
    fn records_survive_a_reopen() {
        let dir =
            std::env::temp_dir().join(format!("remind_me_engine_reopen_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            SavedSearchStore::insert(&mut tables, &saved("ss_1", "one")).unwrap();
            tables
                .mark_seen("ss_1", &["m1".to_string()], "2026-09-25T00:00:00+00:00")
                .unwrap();
        }
        let tables = crate::db::engine::reopen(&dir);
        assert_eq!(
            SavedSearchStore::get(&tables, "ss_1").unwrap(),
            saved("ss_1", "one")
        );
        assert_eq!(
            tables.seen_ids("ss_1").unwrap(),
            HashSet::from(["m1".to_string()])
        );
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unparseable_time_scans_as_zero() {
        assert_eq!(micros("not a time"), 0);
        assert_eq!(micros("1970-01-01T00:00:01+00:00"), 1_000_000);
    }
}
