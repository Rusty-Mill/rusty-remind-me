//! Memory references on SQLite: the [`ReferenceStore`] adapter.

use super::imports::json_list;
use super::{sqlite_error, SqliteTables};
use crate::db::references::{MemoryReference, NewReference, ReferenceStore};
use crate::db::Result;
use rusqlite::{params, Params, Row};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memory_references (
    id TEXT PRIMARY KEY, memory_id TEXT NOT NULL, kind TEXT NOT NULL, value TEXT NOT NULL,
    label TEXT, created_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS memory_references_memory ON memory_references (memory_id);
CREATE INDEX IF NOT EXISTS memory_references_value ON memory_references (kind, value);
";

fn reference(r: &Row<'_>) -> rusqlite::Result<MemoryReference> {
    Ok(MemoryReference {
        id: r.get(0)?,
        memory_id: r.get(1)?,
        kind: r.get(2)?,
        value: r.get(3)?,
        label: r.get(4)?,
        created_at: r.get(5)?,
    })
}

impl SqliteTables {
    fn references(&self, clause: &str, params: impl Params) -> Result<Vec<MemoryReference>> {
        self.all(
            &format!(
                "SELECT id, memory_id, kind, value, label, created_at FROM memory_references \
                 WHERE {clause} ORDER BY created_at, kind, value, id"
            ),
            params,
            reference,
        )
    }
}

impl ReferenceStore for SqliteTables {
    fn insert(&mut self, r: &NewReference) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO memory_references VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![r.id, r.memory_id, r.kind, r.value, r.label, r.created_at],
            )
            .map(drop)
            .map_err(sqlite_error)
    }

    fn for_memory(&self, memory_id: &str) -> Result<Vec<MemoryReference>> {
        self.references("memory_id = ?1", [memory_id])
    }

    fn for_memories(&self, memory_ids: &[String]) -> Result<Vec<MemoryReference>> {
        self.references(
            "memory_id IN (SELECT value FROM json_each(?1))",
            [json_list(memory_ids)],
        )
    }

    fn of_kind(&self, kind: &str) -> Result<Vec<MemoryReference>> {
        self.references("kind = ?1", [kind])
    }

    fn find(&self, kind: &str, value: &str) -> Result<Vec<MemoryReference>> {
        self.references("kind = ?1 AND value = ?2", [kind, value])
    }

    fn delete_for_memory(&mut self, memory_id: &str) -> Result<usize> {
        self.exec(
            "DELETE FROM memory_references WHERE memory_id = ?1",
            [memory_id],
        )
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::references::reference_conformance!(SqliteTables::open_in_memory().unwrap());
}
