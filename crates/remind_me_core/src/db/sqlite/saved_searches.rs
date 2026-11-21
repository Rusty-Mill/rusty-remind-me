//! Saved searches and their seen-memory rows on SQLite: the
//! [`SavedSearchStore`] adapter.

use super::{sqlite_error, SqliteTables};
use crate::db::saved_searches::{decode_filters, encode_filters, SavedSearchStore};
use crate::db::{Result, StoreError};
use crate::models::SavedSearch;
use rusqlite::{params, OptionalExtension, Row};
use std::collections::HashSet;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS saved_searches (
    id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, query TEXT NOT NULL,
    filters TEXT NOT NULL, watch INTEGER NOT NULL,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS saved_search_seen (
    saved_search_id TEXT NOT NULL, memory_id TEXT NOT NULL,
    first_seen_at TEXT NOT NULL, PRIMARY KEY (saved_search_id, memory_id));
";

const COLUMNS: &str = "id, name, query, filters, watch, created_at, updated_at";

fn saved(row: &Row<'_>) -> rusqlite::Result<SavedSearch> {
    Ok(SavedSearch {
        id: row.get(0)?,
        name: row.get(1)?,
        query: row.get(2)?,
        filters: decode_filters(&row.get::<_, String>(3)?),
        watch: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

impl SqliteTables {
    fn saved_where(&self, clause: &str, key: &str) -> Result<Option<SavedSearch>> {
        let sql = format!("SELECT {COLUMNS} FROM saved_searches WHERE {clause} = ?1");
        self.conn
            .query_row(&sql, [key], saved)
            .optional()
            .map_err(sqlite_error)
    }
}

impl SavedSearchStore for SqliteTables {
    fn id_for_name(&self, name: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT id FROM saved_searches WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()
            .map_err(sqlite_error)
    }

    fn insert(&mut self, s: &SavedSearch) -> Result<()> {
        if self.id_for_name(&s.name)?.is_some() {
            return Err(StoreError::Invalid(format!(
                "a saved search named {:?} already exists",
                s.name
            )));
        }
        self.atomic(|c| {
            c.execute(
                &format!(
                    "INSERT INTO saved_searches ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                ),
                params![
                    s.id,
                    s.name,
                    s.query,
                    encode_filters(&s.filters),
                    i64::from(s.watch),
                    s.created_at,
                    s.updated_at
                ],
            )
            .map(drop)
            .map_err(sqlite_error)
        })
    }

    fn update(&mut self, s: &SavedSearch) -> Result<()> {
        self.atomic(|c| {
            c.execute(
                    "UPDATE saved_searches SET query = ?2, filters = ?3, watch = ?4, updated_at = ?5 WHERE id = ?1",
                    params![s.id, s.query, encode_filters(&s.filters), i64::from(s.watch), s.updated_at],
                )
                .map(drop)
                .map_err(sqlite_error)
        })
    }

    fn get(&self, id: &str) -> Result<SavedSearch> {
        self.saved_where("id", id)?.ok_or(StoreError::NotFound)
    }

    fn get_by_name(&self, name: &str) -> Result<Option<SavedSearch>> {
        self.saved_where("name", name)
    }

    fn list(&self) -> Result<Vec<SavedSearch>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM saved_searches ORDER BY name"
            ))
            .map_err(sqlite_error)?;
        let rows = stmt.query_map([], saved).map_err(sqlite_error)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sqlite_error)
    }

    fn delete(&mut self, id: &str) -> Result<()> {
        self.atomic(|c| {
            c.execute(
                "DELETE FROM saved_search_seen WHERE saved_search_id = ?1",
                [id],
            )
            .and_then(|_| c.execute("DELETE FROM saved_searches WHERE id = ?1", [id]))
            .map(drop)
            .map_err(sqlite_error)
        })
    }

    fn seen_ids(&self, saved_search_id: &str) -> Result<HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT memory_id FROM saved_search_seen WHERE saved_search_id = ?1")
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([saved_search_id], |r| r.get(0))
            .map_err(sqlite_error)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sqlite_error)
    }

    fn has_any_seen(&self, saved_search_id: &str) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM saved_search_seen WHERE saved_search_id = ?1)",
                [saved_search_id],
                |r| r.get(0),
            )
            .map_err(sqlite_error)
    }

    fn mark_seen(&mut self, saved_search_id: &str, memory_ids: &[String], now: &str) -> Result<()> {
        self.atomic(|c| {
            let mut stmt = c
                .prepare("INSERT OR IGNORE INTO saved_search_seen VALUES (?1, ?2, ?3)")
                .map_err(sqlite_error)?;
            for memory_id in memory_ids {
                stmt.execute(params![saved_search_id, memory_id, now])
                    .map_err(sqlite_error)?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::saved_searches::saved_search_conformance!(SqliteTables::open_in_memory().unwrap());
}
