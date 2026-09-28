//! Copying a node's SQLite store onto the engine (ADR-0023 §5, phase 5.1):
//! the memories core's sixteen tables.
//!
//! The copy follows the hub copy's three rules:
//! - it never writes to the source: it reads `SELECT *` from each table and
//!   nothing else;
//! - it refuses rows the engine cannot store rather than dropping them
//!   silently: a row whose columns do not fit the engine record (a NULL
//!   where the engine keeps text, a value of the wrong type), or that would
//!   land on a key an earlier row already took, is left out and reported in
//!   [`CopyReport::refused`];
//! - it verifies every row after writing: each stored record is read back
//!   and compared with what was written, and each record's columns with
//!   the source row's.
//!
//! Every row keeps its ids: records are keyed exactly as the write paths
//! key them, so a copied store reads as the source did.

use super::core::{encode, Change, CoreTables};
use super::memories::{MemoryRecord, MemoryRow};
use super::page::before_image;
use super::{engine_error, engine_id, outbox, pair_engine_id, EngineTables};
use crate::db::migrations::SCHEMA_VERSION;
use crate::db::{Result, StoreError};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension};
use rusty_multimodal_db_engine::generic::query::AllIds;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// How many records one journal batch carries while copying.
const BATCH: usize = 500;

/// A source row the copy left out, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub table: &'static str,
    /// The row's key columns, as `column=value` pairs.
    pub key: String,
    pub reason: String,
}

/// What a copy did: rows copied per table, and rows refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyReport {
    pub copied: BTreeMap<&'static str, usize>,
    pub refused: Vec<Refused>,
}

/// One source row, column name to value.
type Row = Map<String, Value>;

/// How one source table becomes engine records.
struct TableCopy {
    table: &'static str,
    /// The columns that name a row in a refusal.
    key_columns: &'static [&'static str],
    /// Source columns the engine does not keep, so verification skips them.
    dropped: &'static [&'static str],
    build: fn(Row) -> std::result::Result<Change, String>,
}

/// The core's tables, in the order they are copied.
const TABLES: &[TableCopy] = &[
    TableCopy {
        table: "memories",
        key_columns: &["id"],
        dropped: &[],
        build: memory,
    },
    TableCopy {
        table: "sync_outbox",
        key_columns: &["id"],
        dropped: &[],
        build: |row| {
            let id = int(&row, "id")?;
            Ok(Change::Outbox(id, Some(record(row)?)))
        },
    },
    TableCopy {
        table: "sync_sends",
        key_columns: &["remote_id", "outbox_id"],
        dropped: &[],
        build: |row| {
            let id = pair_engine_id(
                &text(&row, "remote_id")?,
                &int(&row, "outbox_id")?.to_string(),
            );
            Ok(Change::Send(id, Some(keyed(row, id)?)))
        },
    },
    TableCopy {
        table: "sync_flags",
        key_columns: &["key"],
        dropped: &[],
        build: |row| {
            let id = engine_id(&text(&row, "key")?);
            Ok(Change::Flag(id, Some(keyed(row, id)?)))
        },
    },
    TableCopy {
        table: "reminder_deliveries",
        key_columns: &["memory_id", "remind_at"],
        // The engine keys a delivery by (memory, remind_at); the rowid the
        // SQL gave it is never read.
        dropped: &["id"],
        build: |row| {
            let id = pair_engine_id(&text(&row, "memory_id")?, &text(&row, "remind_at")?);
            Ok(Change::Delivery(id, Some(keyed(row, id)?)))
        },
    },
    TableCopy {
        table: "memory_feedback",
        key_columns: &["id"],
        dropped: &[],
        build: |row| {
            let id = engine_id(&text(&row, "id")?);
            Ok(Change::Feedback(id, Some(keyed(row, id)?)))
        },
    },
    TableCopy {
        table: "entities",
        key_columns: &["id"],
        dropped: &[],
        build: |row| {
            let id = engine_id(&text(&row, "id")?);
            Ok(Change::Entity(id, Some(Box::new(keyed(row, id)?))))
        },
    },
    TableCopy {
        table: "memory_entities",
        key_columns: &["memory_id", "entity_id"],
        dropped: &[],
        build: |row| {
            let id = pair_engine_id(&text(&row, "memory_id")?, &text(&row, "entity_id")?);
            Ok(Change::Link(id, Some(keyed(row, id)?)))
        },
    },
    TableCopy {
        table: "entity_relations",
        key_columns: &["id"],
        dropped: &[],
        build: |row| {
            let id = engine_id(&text(&row, "id")?);
            Ok(Change::Relation(id, Some(Box::new(keyed(row, id)?))))
        },
    },
    TableCopy {
        table: "memory_associations",
        key_columns: &["memory_id_a", "memory_id_b"],
        dropped: &[],
        build: |row| {
            let id = pair_engine_id(&text(&row, "memory_id_a")?, &text(&row, "memory_id_b")?);
            Ok(Change::Association(id, Some(keyed(row, id)?)))
        },
    },
    TableCopy {
        table: "promotions",
        key_columns: &["promoted_id", "source_id"],
        dropped: &[],
        build: |row| {
            let id = pair_engine_id(&text(&row, "promoted_id")?, &text(&row, "source_id")?);
            Ok(Change::Promotion(id, Some(keyed(row, id)?)))
        },
    },
    TableCopy {
        table: "vec_chunks",
        key_columns: &["memory_id", "chunk_ix"],
        dropped: &[],
        build: |row| {
            let id = pair_engine_id(
                &text(&row, "memory_id")?,
                &int(&row, "chunk_ix")?.to_string(),
            );
            Ok(Change::Chunk(id, Some(Box::new(keyed(row, id)?))))
        },
    },
    TableCopy {
        table: "embedding_meta",
        key_columns: &["key"],
        dropped: &[],
        build: |row| {
            let id = engine_id(&text(&row, "key")?);
            Ok(Change::EmbeddingMeta(id, Some(keyed(row, id)?)))
        },
    },
    TableCopy {
        table: "chat_imports",
        key_columns: &["import_id"],
        dropped: &[],
        build: |row| {
            let id = engine_id(&text(&row, "import_id")?);
            Ok(Change::ChatImport(id, Some(Box::new(keyed(row, id)?))))
        },
    },
    TableCopy {
        table: "dbs_imports",
        key_columns: &["dbs_source", "external_id"],
        dropped: &[],
        build: |row| {
            let id = pair_engine_id(&text(&row, "dbs_source")?, &text(&row, "external_id")?);
            Ok(Change::DbsImport(id, Some(Box::new(keyed(row, id)?))))
        },
    },
    TableCopy {
        table: "mempalace_imports",
        key_columns: &["drawer_id"],
        dropped: &[],
        build: |row| {
            let id = engine_id(&text(&row, "drawer_id")?);
            Ok(Change::MempalaceImport(id, Some(keyed(row, id)?)))
        },
    },
];

fn text(row: &Row, column: &str) -> std::result::Result<String, String> {
    match row.get(column) {
        Some(Value::String(s)) => Ok(s.clone()),
        other => Err(format!("{column} is {other:?}, not text")),
    }
}

fn int(row: &Row, column: &str) -> std::result::Result<i64, String> {
    row.get(column)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{column} is {:?}, not an integer", row.get(column)))
}

/// `row` as the engine record `R`.
fn record<R: DeserializeOwned>(row: Row) -> std::result::Result<R, String> {
    serde_json::from_value(Value::Object(row)).map_err(|e| e.to_string())
}

/// [`record`], with the engine id and an unused slot added.
fn keyed<R: DeserializeOwned>(mut row: Row, id: Uuid) -> std::result::Result<R, String> {
    row.insert("engine_id".to_string(), Value::String(id.to_string()));
    row.entry("slot").or_insert(Value::from(0));
    record(row)
}

/// A memory row: the flag is an integer in SQLite and a boolean on the row.
fn memory(mut row: Row) -> std::result::Result<Change, String> {
    if let Some(Value::Number(flag)) = row.get("sensitive").cloned() {
        row.insert(
            "sensitive".to_string(),
            Value::Bool(flag.as_i64().unwrap_or(0) != 0),
        );
    }
    let memory: MemoryRow = record(row)?;
    let record = MemoryRecord::new(memory);
    let id = engine_id(&record.row.id);
    Ok(Change::Memory(id, Some(Box::new(record))))
}

/// A SQLite value as JSON.
fn json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(i) => Value::from(i),
        SqlValue::Real(f) => Value::from(f),
        SqlValue::Text(s) => Value::String(s),
        SqlValue::Blob(b) => Value::from(b),
    }
}

/// Every row of `table`, or none when the source has no such table (the
/// `promotions` table is created on first use).
fn rows(source: &Connection, table: &str) -> Result<Vec<Row>> {
    let exists: Option<String> = source
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
            [table],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(Vec::new());
    }
    let mut stmt = source.prepare(&format!("SELECT * FROM {table}"))?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let rows = stmt
        .query_map([], |r| {
            let mut row = Row::new();
            for (i, name) in names.iter().enumerate() {
                row.insert(name.clone(), json(r.get::<_, SqlValue>(i)?));
            }
            Ok(row)
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn describe(copy: &TableCopy, row: &Row) -> String {
    copy.key_columns
        .iter()
        .map(|c| format!("{c}={}", row.get(*c).unwrap_or(&Value::Null)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The store and key a change writes, as the journal names them.
fn key_of(change: &Change) -> Result<(String, Vec<u8>)> {
    let batch = encode(std::slice::from_ref(change))?;
    let first = batch
        .changes
        .into_iter()
        .next()
        .ok_or_else(|| StoreError::Engine("a change encoded to nothing".to_string()))?;
    Ok((first.store, first.key))
}

/// The record a change writes, as its columns: a memory's row flattened,
/// its flag as 0 or 1, as SQLite stores it.
fn columns(change: &Change) -> Result<Row> {
    let batch = encode(std::slice::from_ref(change))?;
    let value = batch
        .changes
        .into_iter()
        .next()
        .and_then(|c| c.value)
        .ok_or_else(|| StoreError::Engine("a copied change writes nothing".to_string()))?;
    let Value::Object(mut map) = serde_json::from_slice(&value).map_err(engine_error)? else {
        return Err(StoreError::Engine("a record is not an object".to_string()));
    };
    if let Some(Value::Object(row)) = map.remove("row") {
        map = row;
    }
    if let Some(Value::Bool(flag)) = map.get("sensitive").cloned() {
        map.insert("sensitive".to_string(), Value::from(i64::from(flag)));
    }
    Ok(map)
}

/// Whether two column values are the same: equal, or equal numbers.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// The columns of `row` the record `built` does not carry exactly.
fn lost_columns(copy: &TableCopy, row: &Row, built: &Row) -> Vec<String> {
    row.iter()
        .filter(|(column, _)| !copy.dropped.contains(&column.as_str()))
        .filter(|(column, value)| !built.get(*column).is_some_and(|b| same(value, b)))
        .map(|(column, value)| format!("{column}={value}"))
        .collect()
}

/// Whether the core already holds anything: the copy only fills an empty
/// core. The sync flags do not count: opening the tables already sets the
/// `sync_enabled` gate, and the source's flags replace what is there.
fn has_rows(core: &CoreTables) -> bool {
    !(core.memories.all_ids().is_empty()
        && core.outbox.all_ids().is_empty()
        && core.sends.all_ids().is_empty()
        && core.deliveries.all_ids().is_empty()
        && core.feedback.all_ids().is_empty()
        && core.entities.all_ids().is_empty()
        && core.links.all_ids().is_empty()
        && core.relations.all_ids().is_empty()
        && core.associations.all_ids().is_empty()
        && core.promotions.all_ids().is_empty()
        && core.chunks.all_ids().is_empty()
        && core.embedding_meta.all_ids().is_empty()
        && core.chat_imports.all_ids().is_empty()
        && core.dbs_imports.all_ids().is_empty()
        && core.mempalace_imports.all_ids().is_empty())
}

/// Copy the memories core of the SQLite store `source` into the empty core
/// of `target`, keeping every id.
///
/// # Errors
///
/// [`StoreError::Invalid`] if the source is not at this build's schema
/// version (the copy never migrates it) or the target core is not empty;
/// [`StoreError::Engine`] if a written record does not read back as it was
/// written; the source's or the engine's own error otherwise. A refused row
/// is not an error: it is in the report.
pub fn copy_core(source: &Connection, target: &mut EngineTables) -> Result<CopyReport> {
    let version: i32 = source.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(StoreError::Invalid(format!(
            "the source store is at schema version {version}, not {SCHEMA_VERSION}; \
             open it with this build first"
        )));
    }
    if has_rows(&target.core) {
        return Err(StoreError::Invalid(
            "the target memories core is not empty; the copy fills an empty one".to_string(),
        ));
    }
    let mut report = CopyReport::default();
    for copy in TABLES {
        let copied = copy_table(source, target, copy, &mut report.refused)?;
        report.copied.insert(copy.table, copied);
    }
    let floor = outbox::max_id(&target.core.outbox);
    target.journal.raise_to(outbox::SEQUENCE, floor);
    Ok(report)
}

/// Copy one table, verify it, and return how many rows it copied.
fn copy_table(
    source: &Connection,
    target: &mut EngineTables,
    copy: &TableCopy,
    refused: &mut Vec<Refused>,
) -> Result<usize> {
    let mut changes = Vec::new();
    let mut taken: HashMap<(String, Vec<u8>), String> = HashMap::new();
    for row in rows(source, copy.table)? {
        let key = describe(copy, &row);
        let mut refuse = |reason: String| {
            refused.push(Refused {
                table: copy.table,
                key: key.clone(),
                reason,
            })
        };
        let change = match (copy.build)(row.clone()) {
            Ok(change) => change,
            Err(reason) => {
                refuse(reason);
                continue;
            }
        };
        let lost = lost_columns(copy, &row, &columns(&change)?);
        if !lost.is_empty() {
            refuse(format!("the engine cannot keep {}", lost.join(", ")));
            continue;
        }
        let slot = key_of(&change)?;
        if let Some(earlier) = taken.get(&slot) {
            refuse(format!("its key is already taken by the row {earlier}"));
            continue;
        }
        taken.insert(slot, key);
        changes.push(change);
    }
    let copied = changes.len();
    for chunk in changes.chunks(BATCH) {
        target.commit(chunk.to_vec())?;
        for change in chunk {
            verify(&target.core, copy, change)?;
        }
    }
    Ok(copied)
}

/// Fail unless the core holds exactly what `change` wrote.
fn verify(core: &CoreTables, copy: &TableCopy, change: &Change) -> Result<()> {
    let stored = before_image(core, change);
    if &stored == change {
        return Ok(());
    }
    Err(StoreError::Engine(format!(
        "a copied {} record did not read back as written: wrote {change:?}, read {stored:?}",
        copy.table
    )))
}

#[cfg(test)]
mod tests;
