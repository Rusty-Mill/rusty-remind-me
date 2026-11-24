//! [`TestRows`] over SQLite's tables (D42): the raw statements
//! `crate::testing`'s helpers stand for.

use super::memories::{replace, stored};
use super::{all, sqlite_error, SqliteTables};
use crate::db::engine::testing::{columns, with_column};
use crate::db::{Result, StoreError};
use crate::testing::{stored_columns, OutboxRow, RowVisitor, Table, TestRows};
use rusqlite::params;
use rusqlite::types::ValueRef;
use serde_json::Value;

/// The table `table` names.
fn table_name(table: Table) -> &'static str {
    match table {
        Table::Memories => "memories",
        Table::SyncOutbox => "sync_outbox",
        Table::SyncSends => "sync_sends",
        Table::ReminderDeliveries => "reminder_deliveries",
        Table::MemoryFeedback => "memory_feedback",
        Table::Entities => "entities",
        Table::MemoryEntities => "memory_entities",
        Table::EntityRelations => "entity_relations",
        Table::MemoryAssociations => "memory_associations",
        Table::Promotions => "promotions",
        Table::VecChunks => "vec_chunks",
        Table::ChatImports => "chat_imports",
        Table::DbsImports => "dbs_imports",
        Table::MempalaceImports => "mempalace_imports",
    }
}

/// A stored value as JSON: a blob as an array of bytes, as the engine's
/// records serialize one.
fn json_of(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Value::from(f),
        ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::from(b.to_vec()),
    }
}

impl TestRows for SqliteTables {
    fn memory_column(&self, id: &str, column: &str) -> Result<Option<Value>> {
        let Some(row) = stored(&self.conn, id)? else {
            return Ok(None);
        };
        Ok(Some(columns(&row)?.remove(column).unwrap_or(Value::Null)))
    }

    fn set_memory_column(&mut self, id: &str, column: &str, value: Value) -> Result<usize> {
        self.atomic(|c| {
            let Some(row) = stored(c, id)? else {
                return Ok(0);
            };
            replace(c, &row, &with_column(&row, column, value)?)?;
            Ok(1)
        })
    }

    fn relabel_memory(&mut self, from: &str, to: &str) -> Result<usize> {
        self.atomic(|c| {
            let Some(row) = stored(c, from)? else {
                return Ok(0);
            };
            if stored(c, to)?.is_some() {
                return Err(StoreError::Invalid(format!("memory {to:?} already exists")));
            }
            let mut moved = row.clone();
            moved.id = to.to_string();
            replace(c, &row, &moved)?;
            Ok(1)
        })
    }

    fn memory_ids(&self) -> Result<Vec<String>> {
        self.all("SELECT id FROM memories ORDER BY id", [], |r| r.get(0))
    }

    fn count(&self, table: Table) -> Result<i64> {
        let sql = format!("SELECT count(*) FROM {}", table_name(table));
        Ok(self.one(&sql, [], |r| r.get(0))?.unwrap_or(0))
    }

    fn feedback_queries(&self, memory_id: &str) -> Result<Vec<String>> {
        self.all(
            "SELECT query FROM memory_feedback WHERE memory_id = ?1 ORDER BY created_at, id",
            [memory_id],
            |r| r.get(0),
        )
    }

    fn association_weight(&self, a: &str, b: &str) -> Result<Option<i64>> {
        self.one(
            "SELECT weight FROM memory_associations WHERE memory_id_a = ?1 AND memory_id_b = ?2",
            [a, b],
            |r| r.get(0),
        )
    }

    fn outbox_rows(&self) -> Result<Vec<OutboxRow>> {
        all(
            &self.conn,
            "SELECT id, memory_id, operation, payload, created_at, sent_at FROM sync_outbox ORDER BY id",
            [],
            |r| {
                Ok(OutboxRow {
                    id: r.get(0)?,
                    memory_id: r.get(1)?,
                    operation: r.get(2)?,
                    payload: r.get(3)?,
                    created_at: r.get(4)?,
                    sent_at: r.get(5)?,
                })
            },
        )
    }

    fn set_outbox_column(&mut self, id: i64, column: &str, value: &str) -> Result<usize> {
        let sql = match column {
            "created_at" => "UPDATE sync_outbox SET created_at = ?1 WHERE id = ?2",
            _ => "UPDATE sync_outbox SET sent_at = ?1 WHERE id = ?2",
        };
        self.exec(sql, params![value, id])
    }

    fn sends(&self) -> Result<Vec<(String, i64, String)>> {
        self.all(
            "SELECT remote_id, outbox_id, sent_at FROM sync_sends ORDER BY remote_id, outbox_id, sent_at",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    }

    fn each_stored_row(&self, table: Table, f: &mut RowVisitor<'_>) -> Result<()> {
        let names = stored_columns(table)
            .ok_or_else(|| StoreError::Invalid(format!("{table:?} rows are not read here")))?;
        let sql = format!("SELECT {} FROM {}", names.join(", "), table_name(table));
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let mut rows = stmt.query([]).map_err(sqlite_error)?;
        while let Some(r) = rows.next().map_err(sqlite_error)? {
            let row = names
                .iter()
                .enumerate()
                .map(|(ix, name)| Ok((name.to_string(), json_of(r.get_ref(ix)?))))
                .collect::<rusqlite::Result<_>>()
                .map_err(sqlite_error)?;
            f(row)?;
        }
        Ok(())
    }

    fn queue_outbox(
        &mut self,
        key: &str,
        operation: &str,
        payload: &str,
        created_at: &str,
    ) -> Result<i64> {
        self.exec(
            "INSERT INTO sync_outbox (memory_id, operation, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![key, operation, payload, created_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
}
