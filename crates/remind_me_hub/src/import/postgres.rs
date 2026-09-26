//! Read a Postgres hub (`DATABASE_URL`) without writing to it, including a
//! legacy database the Python hub left behind.
//!
//! Behind the `postgres-import` feature. It outlived the Postgres store
//! (ADR-0021, decision 6), so a hub that migrates late is not stranded.
//!
//! Each row is read as `to_jsonb(row)`, whatever the schema. That needs no
//! knowledge of which columns exist, and renders a legacy `TIMESTAMPTZ` as
//! ISO-8601 text with its offset, which `record::parse` canonicalises like
//! any pushed timestamp. The retired Postgres store's `migrate` converted
//! such a column in place; this reads it without touching it.

use super::RawTables;
use crate::store::multimodal::snapshot::Snapshot;
use crate::store::{StoreError, StoreResult};
use postgres::{Client, IsolationLevel, NoTls, Transaction};
use serde_json::{Map, Value};

fn err(e: postgres::Error) -> StoreError {
    StoreError(e.to_string())
}

/// Every row of the hub at `url`, read in one repeatable-read, read-only
/// transaction, so the four tables and the sequence agree.
///
/// # Errors
///
/// Fails if the server cannot be reached or a table cannot be read. A
/// missing table reads as empty.
pub fn read(url: &str) -> StoreResult<Snapshot> {
    let mut client = Client::connect(url, NoTls).map_err(err)?;
    let mut tx = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .map_err(err)?;
    let raw = RawTables {
        memories: table(&mut tx, "memories")?,
        entities: table(&mut tx, "entities")?,
        links: table(&mut tx, "memory_entities")?,
        relations: table(&mut tx, "entity_relations")?,
        seq_high_water: sequence_high_water(&mut tx)?,
    };
    tx.commit().map_err(err)?;
    Ok(raw.into_snapshot())
}

/// Every row of `name` as a column-keyed JSON object, or none if the table
/// does not exist.
fn table(tx: &mut Transaction<'_>, name: &str) -> StoreResult<Vec<Map<String, Value>>> {
    let exists: bool = tx
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&name])
        .map_err(err)?
        .get(0);
    if !exists {
        return Ok(Vec::new());
    }
    // `name` is one of four literals above, never input.
    let rows = tx
        .query(&format!("SELECT to_jsonb(t) FROM {name} t"), &[])
        .map_err(err)?;
    rows.iter()
        .map(|row| match row.get::<_, Value>(0) {
            Value::Object(object) => Ok(object),
            other => Err(StoreError(format!(
                "{name}: a row read as {other}, not an object"
            ))),
        })
        .collect()
}

/// The highest `hub_seq` the `memories_hub_seq` sequence has handed out,
/// which can be above every remaining row: `nextval()` spends a number on
/// a push that loses last-write-wins, and a compaction deletes rows. Zero
/// when there is no sequence (a legacy database) or it was never used.
fn sequence_high_water(tx: &mut Transaction<'_>) -> StoreResult<i64> {
    let exists: bool = tx
        .query_one("SELECT to_regclass('memories_hub_seq') IS NOT NULL", &[])
        .map_err(err)?
        .get(0);
    if !exists {
        return Ok(0);
    }
    let row = tx
        .query_one("SELECT last_value, is_called FROM memories_hub_seq", &[])
        .map_err(err)?;
    let (last_value, is_called): (i64, bool) = (row.get(0), row.get(1));
    Ok(if is_called { last_value } else { 0 })
}
