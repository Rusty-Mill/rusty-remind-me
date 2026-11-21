//! The core side of [`crate::testing`]: raw reads and writes of stored rows.

use super::core::{Change, CoreTables};
use super::memories::{self, MemoryRecord, MemoryRow};
use super::{core_ref, engine_id, EngineTables};
#[cfg(test)]
use crate::db::backend::BackendGuard;
use crate::db::{Result, StoreError};
use crate::testing::{OutboxRow, Table, TestRows};
use rusty_multimodal_db_engine::generic::query::{AllIds, GetById};
use serde_json::Value;

/// The engine behind `guard`, for engine-only tests: they refuse, naming
/// why, on any other backend.
#[cfg(test)]
pub(crate) fn tables<'g>(guard: &'g mut BackendGuard<'_>) -> Result<&'g mut EngineTables> {
    let backend: &mut dyn std::any::Any = &mut **guard;
    backend.downcast_mut::<EngineTables>().ok_or_else(|| {
        StoreError::Invalid("raw engine rows were asked of a store that is not the engine".into())
    })
}

/// The row's columns as JSON, with the flag as 0 or 1 as SQLite stores it.
pub(crate) fn columns(row: &MemoryRow) -> Result<serde_json::Map<String, Value>> {
    let Value::Object(mut map) = serde_json::to_value(row).map_err(super::engine_error)? else {
        return Err(StoreError::Engine(
            "a memory row is not an object".to_string(),
        ));
    };
    if let Some(Value::Bool(flag)) = map.get("sensitive").cloned() {
        map.insert("sensitive".to_string(), Value::from(i64::from(flag)));
    }
    Ok(map)
}

/// `row` with `column` set to `value`, a flag given as 0 or 1.
pub(crate) fn with_column(row: &MemoryRow, column: &str, value: Value) -> Result<MemoryRow> {
    let mut map = columns(row)?;
    map.insert(column.to_string(), value);
    if let Some(Value::Number(flag)) = map.get("sensitive").cloned() {
        map.insert(
            "sensitive".to_string(),
            Value::Bool(flag.as_i64().unwrap_or(0) != 0),
        );
    }
    serde_json::from_value(Value::Object(map))
        .map_err(|e| StoreError::Invalid(format!("memories.{column} cannot hold that: {e}")))
}

impl TestRows for EngineTables {
    fn memory_column(&self, id: &str, column: &str) -> Result<Option<Value>> {
        let Some(row) = memories::row(core_ref(self)?, id) else {
            return Ok(None);
        };
        Ok(Some(columns(&row)?.remove(column).unwrap_or(Value::Null)))
    }

    fn set_memory_column(&mut self, id: &str, column: &str, value: Value) -> Result<usize> {
        let Some(row) = memories::row(core_ref(self)?, id) else {
            return Ok(0);
        };
        let record = MemoryRecord::new(with_column(&row, column, value)?);
        self.commit(vec![Change::Memory(engine_id(id), Some(Box::new(record)))])?;
        Ok(1)
    }

    fn relabel_memory(&mut self, from: &str, to: &str) -> Result<usize> {
        let core = core_ref(self)?;
        let Some(mut row) = memories::row(core, from) else {
            return Ok(0);
        };
        if memories::row(core, to).is_some() {
            return Err(StoreError::Invalid(format!("memory {to:?} already exists")));
        }
        row.id = to.to_string();
        self.commit(vec![
            Change::Memory(engine_id(from), None),
            Change::Memory(engine_id(to), Some(Box::new(MemoryRecord::new(row)))),
        ])?;
        Ok(1)
    }

    fn memory_ids(&self) -> Result<Vec<String>> {
        let mut ids: Vec<String> = memories::rows(core_ref(self)?).map(|r| r.id).collect();
        ids.sort();
        Ok(ids)
    }

    fn count(&self, table: Table) -> Result<i64> {
        let core: &CoreTables = core_ref(self)?;
        let n = match table {
            Table::Memories => core.memories.all_ids().len(),
            Table::SyncOutbox => core.outbox.all_ids().len(),
            Table::SyncSends => core.sends.all_ids().len(),
            Table::ReminderDeliveries => core.deliveries.all_ids().len(),
            Table::MemoryFeedback => core.feedback.all_ids().len(),
            Table::Entities => core.entities.all_ids().len(),
            Table::MemoryEntities => core.links.all_ids().len(),
            Table::EntityRelations => core.relations.all_ids().len(),
            Table::MemoryAssociations => core.associations.all_ids().len(),
            Table::Promotions => core.promotions.all_ids().len(),
            Table::VecChunks => core.chunks.all_ids().len(),
            Table::ChatImports => core.chat_imports.all_ids().len(),
            Table::DbsImports => core.dbs_imports.all_ids().len(),
            Table::MempalaceImports => core.mempalace_imports.all_ids().len(),
        };
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    fn feedback_queries(&self, memory_id: &str) -> Result<Vec<String>> {
        let core: &CoreTables = core_ref(self)?;
        let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let mut events: Vec<(String, String, String)> = core
            .feedback
            .all_ids()
            .into_iter()
            .filter_map(|id| core.feedback.get(id))
            .map(|e| serde_json::to_value(e).map_err(super::engine_error))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|v| field(v, "memory_id") == memory_id)
            .map(|v| (field(&v, "created_at"), field(&v, "id"), field(&v, "query")))
            .collect();
        events.sort();
        Ok(events.into_iter().map(|(_, _, query)| query).collect())
    }

    fn association_weight(&self, a: &str, b: &str) -> Result<Option<i64>> {
        let core: &CoreTables = core_ref(self)?;
        let Some(pair) = core.associations.get(super::pair_engine_id(a, b)) else {
            return Ok(None);
        };
        let pair = serde_json::to_value(pair).map_err(super::engine_error)?;
        let same = pair.get("memory_id_a").and_then(Value::as_str) == Some(a)
            && pair.get("memory_id_b").and_then(Value::as_str) == Some(b);
        Ok(same
            .then(|| pair.get("weight").and_then(Value::as_i64))
            .flatten())
    }

    fn outbox_rows(&self) -> Result<Vec<OutboxRow>> {
        Ok(super::outbox::entries(core_ref(self)?)
            .into_iter()
            .map(|e| OutboxRow {
                id: e.id,
                memory_id: e.memory_id,
                operation: e.operation,
                payload: e.payload,
                created_at: e.created_at,
                sent_at: e.sent_at,
            })
            .collect())
    }

    fn set_outbox_column(&mut self, id: i64, column: &str, value: &str) -> Result<usize> {
        let entries = super::outbox::entries(core_ref(self)?);
        let Some(mut record) = entries.into_iter().find(|e| e.id == id) else {
            return Ok(0);
        };
        match column {
            "created_at" => record.created_at = value.to_string(),
            _ => record.sent_at = value.to_string(),
        }
        self.commit(vec![Change::Outbox(id, Some(record))])?;
        Ok(1)
    }

    fn sends(&self) -> Result<Vec<(String, i64, String)>> {
        super::outbox::send_markers(self)
    }

    fn queue_outbox(
        &mut self,
        key: &str,
        operation: &str,
        payload: &str,
        created_at: &str,
    ) -> Result<i64> {
        super::outbox::queue_at(self, key, operation, payload, created_at)
    }
}
