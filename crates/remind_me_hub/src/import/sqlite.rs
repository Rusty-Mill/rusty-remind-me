//! Read a SQLite hub database (`REMIND_ME_HUB_DB_PATH`) without writing to it.

use super::RawTables;
use crate::store::multimodal::snapshot::Snapshot;
use crate::store::{StoreError, StoreResult};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Number, Value};
use std::path::Path;

fn err(e: rusqlite::Error) -> StoreError {
    StoreError(e.to_string())
}

/// Every row of the hub database at `path`, read in one transaction.
///
/// The database is opened read-only; stop the hub first so the copy is of
/// a hub nobody is writing to.
///
/// # Errors
///
/// Fails if the file cannot be opened read-only or a table cannot be read.
/// A missing table reads as empty.
pub fn read(path: &Path) -> StoreResult<Snapshot> {
    if !path.exists() {
        return Err(StoreError(format!("{} does not exist", path.display())));
    }
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(err)?;
    let tx = conn.transaction().map_err(err)?;
    let raw = RawTables {
        memories: table(&tx, "memories")?,
        entities: table(&tx, "entities")?,
        links: table(&tx, "memory_entities")?,
        relations: table(&tx, "entity_relations")?,
        seq_high_water: high_water(&tx)?,
    };
    tx.finish().map_err(err)?;
    Ok(raw.into_snapshot())
}

fn table_exists(conn: &Connection, name: &str) -> StoreResult<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )
    .map_err(err)
}

/// The highest `hub_seq` the SQLite store has issued, from its `hub_meta`
/// table. It can be above every remaining row: compaction deletes rows,
/// the newest included. Zero for a database from before `hub_meta`
/// existed, whose rows' own highest `hub_seq` is the mark, and which
/// [`RawTables::into_snapshot`] takes into account anyway.
fn high_water(conn: &Connection) -> StoreResult<i64> {
    if !table_exists(conn, "hub_meta")? {
        return Ok(0);
    }
    conn.query_row(
        "SELECT COALESCE((SELECT high_water FROM hub_meta WHERE id = 1), 0)",
        [],
        |row| row.get(0),
    )
    .map_err(err)
}

/// Every row of `name` as a column-keyed JSON object, or none if the table
/// does not exist.
fn table(conn: &Connection, name: &str) -> StoreResult<Vec<Map<String, Value>>> {
    if !table_exists(conn, name)? {
        return Ok(Vec::new());
    }
    // `name` is one of four literals above, never input.
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {name}"))
        .map_err(err)?;
    let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let rows = stmt
        .query_map([], |row| {
            let mut object = Map::new();
            for (i, column) in columns.iter().enumerate() {
                object.insert(column.clone(), json_value(row.get_ref(i)?));
            }
            Ok(object)
        })
        .map_err(err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(err)
}

/// A column value as JSON. TEXT holding JSON (`tags`, `metadata`,
/// `aliases`) stays a string; `record::parse` decodes it as a push would.
fn json_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null | ValueRef::Blob(_) => Value::Null,
        ValueRef::Integer(i) => Value::Number(i.into()),
        ValueRef::Real(f) => Number::from_f64(f).map_or(Value::Null, Value::Number),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
    }
}
