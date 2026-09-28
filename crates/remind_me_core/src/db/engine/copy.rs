//! Copying a node's SQLite store onto the engine (ADR-0023 §5): the
//! memories core's sixteen tables (phase 5.1) and the other groups the
//! engine holds: saved searches, import archives, the sync log, analytics
//! snapshots, memory revisions and the wiki (phase 5.2).
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
use super::{
    analytics, archives, engine_error, engine_id, micros, outbox, pair_engine_id, revisions,
    saved_searches, sync_log, wiki, EngineTables,
};
use crate::db::migrations::SCHEMA_VERSION;
use crate::db::{Result, StoreError};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension};
use rusty_multimodal_db_engine::generic::mmap_field::MmapFieldValue;
use rusty_multimodal_db_engine::generic::query::{AllIds, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
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
    rows_ordered(source, table, "")
}

/// [`rows`], in the order `order_by` gives (an SQL `ORDER BY` clause, or
/// empty).
fn rows_ordered(source: &Connection, table: &str, order_by: &str) -> Result<Vec<Row>> {
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
    let mut stmt = source.prepare(&format!("SELECT * FROM {table} {order_by}"))?;
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

fn describe(key_columns: &[&str], row: &Row) -> String {
    key_columns
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
fn lost_columns(dropped: &[&str], row: &Row, built: &Row) -> Vec<String> {
    row.iter()
        .filter(|(column, _)| !dropped.contains(&column.as_str()))
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
    check_source(source)?;
    if has_rows(&target.core) {
        return Err(StoreError::Invalid(
            "the target memories core is not empty; the copy fills an empty one".to_string(),
        ));
    }
    let mut report = CopyReport::default();
    copy_core_tables(source, target, &mut report)?;
    Ok(report)
}

/// Copy a whole SQLite store into empty engine tables, keeping every id:
/// the memories core, then every other group the engine holds. What the
/// copy tool and the first open of an engine store run.
///
/// # Errors
///
/// As [`copy_core`], and [`StoreError::Invalid`] if any table is not
/// empty.
pub fn copy_store(source: &Connection, target: &mut EngineTables) -> Result<CopyReport> {
    check_source(source)?;
    if has_rows(&target.core) || groups_have_rows(target) {
        return Err(StoreError::Invalid(
            "the target engine tables are not empty; the copy fills empty ones".to_string(),
        ));
    }
    let mut report = CopyReport::default();
    copy_core_tables(source, target, &mut report)?;
    copy_groups(source, target, &mut report)?;
    Ok(report)
}

/// Copy the SQLite store at `source` into engine tables at `target_dir`,
/// with [`copy_store`]. The source is opened read-only; the target is
/// created if it does not exist, and must hold no rows if it does.
///
/// # Errors
///
/// As [`copy_store`], and the source's or the engine's error if either
/// cannot be opened.
pub fn copy_file(source: &std::path::Path, target_dir: &std::path::Path) -> Result<CopyReport> {
    let source = Connection::open_with_flags(
        source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut target = EngineTables::open(target_dir)?;
    copy_store(&source, &mut target)
}

/// Copy the SQLite file `source` into the engine directory `dir`, which
/// must not exist yet. The copy goes into a sibling `.partial` directory
/// that is renamed to `dir` only once every row copied, so a crash or a
/// refused row never leaves a half-copied directory where an open would
/// take it for the store. A `.partial` left by an earlier attempt is
/// removed first.
///
/// # Errors
///
/// [`StoreError::Invalid`] when the copy refused a row; the partial copy
/// is left for inspection and removed by the next attempt.
pub fn copy_into_place(source: &std::path::Path, dir: &std::path::Path) -> Result<CopyReport> {
    let mut partial = dir.as_os_str().to_owned();
    partial.push(".partial");
    let partial = std::path::PathBuf::from(partial);
    if partial.exists() {
        std::fs::remove_dir_all(&partial).map_err(|e| io_error(&partial, &e))?;
    }
    let report = copy_file(source, &partial)?;
    if let Some(first) = report.refused.first() {
        return Err(StoreError::Invalid(format!(
            "{} row(s) could not be copied onto the engine, the first from {} [{}]: {}; \
             {} was left as it was",
            report.refused.len(),
            first.table,
            first.key,
            first.reason,
            source.display()
        )));
    }
    std::fs::rename(&partial, dir).map_err(|e| io_error(dir, &e))?;
    sync_parent(dir)?;
    Ok(report)
}

fn io_error(path: &std::path::Path, e: &std::io::Error) -> StoreError {
    StoreError::Engine(format!("{}: {e}", path.display()))
}

/// Make the rename of `dir` durable: on Unix a directory entry is only on
/// disk once its parent directory is synced.
#[cfg(unix)]
fn sync_parent(dir: &std::path::Path) -> Result<()> {
    let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    std::fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| io_error(parent, &e))
}

#[cfg(not(unix))]
fn sync_parent(_dir: &std::path::Path) -> Result<()> {
    Ok(())
}

/// Refuse a source that is not at this build's schema version: the copy
/// never migrates it.
fn check_source(source: &Connection) -> Result<()> {
    let version: i32 = source.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(StoreError::Invalid(format!(
            "the source store is at schema version {version}, not {SCHEMA_VERSION}; \
             open it with this build first"
        )));
    }
    Ok(())
}

fn copy_core_tables(
    source: &Connection,
    target: &mut EngineTables,
    report: &mut CopyReport,
) -> Result<()> {
    for copy in TABLES {
        let copied = copy_table(source, target, copy, &mut report.refused)?;
        report.copied.insert(copy.table, copied);
    }
    let floor = outbox::max_id(&target.core.outbox);
    target.journal.raise_to(outbox::SEQUENCE, floor);
    Ok(())
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
        let key = describe(copy.key_columns, &row);
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
        let lost = lost_columns(copy.dropped, &row, &columns(&change)?);
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

// --- the other groups -----------------------------------------------------

/// Whether any group outside the core already holds rows.
fn groups_have_rows(tables: &EngineTables) -> bool {
    !(tables.saved_searches.all_ids().is_empty()
        && tables.seen.all_ids().is_empty()
        && tables.archives.all_ids().is_empty()
        && tables.spans.all_ids().is_empty()
        && tables.sync_log.all_ids().is_empty()
        && tables.snapshots.all_ids().is_empty()
        && tables.revisions.all_ids().is_empty()
        && tables.wiki_pages.all_ids().is_empty()
        && tables.wiki_links.all_ids().is_empty()
        && tables.wiki_meta.all_ids().is_empty())
}

/// `row`'s value under `column` as a timestamp's µs, for a slot.
fn micros_of(row: &Row, column: &str) -> std::result::Result<i64, String> {
    Ok(micros(&text(row, column)?))
}

/// A nullable flag: SQLite's 0/1 as a boolean, NULL as none.
fn nullable_flag(row: &mut Row, column: &str) {
    if let Some(Value::Number(flag)) = row.get(column).cloned() {
        row.insert(
            column.to_string(),
            Value::Bool(flag.as_i64().unwrap_or(0) != 0),
        );
    }
}

/// Copy every group outside the core, then rebuild what is derived from
/// them and raise their sequences.
fn copy_groups(
    source: &Connection,
    target: &mut EngineTables,
    report: &mut CopyReport,
) -> Result<()> {
    let mut group = Group { source, report };
    group.copy(
        "saved_searches",
        "",
        &["id"],
        &mut target.saved_searches,
        |row| {
            let id = engine_id(&text(&row, "id")?);
            keyed::<saved_searches::SavedSearchRow>(row, id)
        },
    )?;
    group.copy(
        "saved_search_seen_memories",
        "",
        &["saved_search_id", "memory_id"],
        &mut target.seen,
        |mut row| {
            let id = pair_engine_id(&text(&row, "saved_search_id")?, &text(&row, "memory_id")?);
            let us = micros_of(&row, "first_seen_at")?;
            row.insert("first_seen_us".to_string(), Value::from(us));
            keyed::<saved_searches::SeenRow>(row, id)
        },
    )?;
    group.copy(
        "import_archives",
        "",
        &["import_id"],
        &mut target.archives,
        |mut row| {
            let id = engine_id(&text(&row, "import_id")?);
            let us = micros_of(&row, "archived_at")?;
            row.insert("archived_us".to_string(), Value::from(us));
            keyed::<archives::ArchiveRecord>(row, id)
        },
    )?;
    group.copy(
        "import_archive_spans",
        "",
        &["memory_id"],
        &mut target.spans,
        |row| {
            let id = engine_id(&text(&row, "memory_id")?);
            keyed::<archives::SpanRecord>(row, id)
        },
    )?;
    group.copy(
        "sync_log",
        "",
        &["remote_id"],
        &mut target.sync_log,
        |row| {
            let id = engine_id(&text(&row, "remote_id")?);
            keyed::<sync_log::SyncLogRecord>(row, id)
        },
    )?;
    group.copy(
        "analytics_snapshots",
        "",
        &["id"],
        &mut target.snapshots,
        |mut row| {
            let day = analytics::utc_day(&text(&row, "captured_at")?);
            row.insert("day".to_string(), Value::String(day));
            record::<analytics::SnapshotRecord>(row)
        },
    )?;
    group.copy(
        "memory_revisions",
        "",
        &["id"],
        &mut target.revisions,
        |mut row| {
            let us = micros_of(&row, "edited_at")?;
            row.insert("edited_us".to_string(), Value::from(us));
            nullable_flag(&mut row, "sensitive");
            record::<revisions::RevisionRecord>(row)
        },
    )?;
    group.copy(
        "wiki_pages",
        "",
        &["slug"],
        &mut target.wiki_pages,
        |mut row| {
            let id = engine_id(&text(&row, "slug")?);
            let us = micros_of(&row, "updated_at")?;
            row.insert("updated_us".to_string(), Value::from(us));
            keyed::<wiki::PageRecord>(row, id)
        },
    )?;
    // A link's position is its place among its source page's links, in the
    // order SQLite inserted them.
    let mut positions: HashMap<String, i64> = HashMap::new();
    group.copy(
        "wiki_links",
        "ORDER BY rowid",
        &["src_slug", "dst_slug"],
        &mut target.wiki_links,
        |mut row| {
            let src = text(&row, "src_slug")?;
            let id = pair_engine_id(&src, &text(&row, "dst_slug")?);
            let position = positions.entry(src).or_insert(0);
            row.insert("position".to_string(), Value::from(*position));
            *position += 1;
            keyed::<wiki::LinkRecord>(row, id)
        },
    )?;
    group.copy("wiki_meta", "", &["key"], &mut target.wiki_meta, |row| {
        let id = engine_id(&text(&row, "key")?);
        keyed::<wiki::MetaRecord>(row, id)
    })?;

    target.wiki_search = wiki::index_pages(&target.wiki_pages);
    let floor = analytics::max_id(&target.snapshots);
    target.journal.raise_to(analytics::SEQUENCE, floor);
    let floor = revisions::max_id(&target.revisions);
    target.journal.raise_to(revisions::SEQUENCE, floor);
    Ok(())
}

/// The copy of the groups outside the core, which write straight to their
/// stores as their own write paths do.
struct Group<'a> {
    source: &'a Connection,
    report: &'a mut CopyReport,
}

impl Group<'_> {
    /// Copy `table` into `store`: build each row's record, refuse what it
    /// cannot keep, insert the rest, and read each one back.
    fn copy<R, Index, Slot>(
        &mut self,
        table: &'static str,
        order_by: &str,
        key_columns: &'static [&'static str],
        store: &mut GenericMmapStore<R, Index, Slot>,
        mut build: impl FnMut(Row) -> std::result::Result<R, String>,
    ) -> Result<()>
    where
        R: Record
            + IndexedField<Index>
            + ScannableField<Slot>
            + Clone
            + PartialEq
            + std::fmt::Debug
            + Serialize
            + DeserializeOwned
            + SchemaTag,
        R::Id: MmapFieldValue + Serialize + DeserializeOwned + std::fmt::Debug + Eq + Hash,
        R::ScanValue: MmapFieldValue,
    {
        let mut taken: HashMap<R::Id, String> = HashMap::new();
        let mut copied = 0;
        for row in rows_ordered(self.source, table, order_by)? {
            let key = describe(key_columns, &row);
            let mut refuse = |reason: String| {
                self.report.refused.push(Refused {
                    table,
                    key: key.clone(),
                    reason,
                })
            };
            let record = match build(row.clone()) {
                Ok(record) => record,
                Err(reason) => {
                    refuse(reason);
                    continue;
                }
            };
            let lost = lost_columns(&[], &row, &record_columns(&record)?);
            if !lost.is_empty() {
                refuse(format!("the engine cannot keep {}", lost.join(", ")));
                continue;
            }
            if let Some(earlier) = taken.get(&record.id()) {
                refuse(format!("its key is already taken by the row {earlier}"));
                continue;
            }
            taken.insert(record.id(), key);
            store.insert(record.clone()).map_err(engine_error)?;
            let stored = store.get(record.id());
            if stored.as_ref() != Some(&record) {
                return Err(StoreError::Engine(format!(
                    "a copied {table} record did not read back as written: \
                     wrote {record:?}, read {stored:?}"
                )));
            }
            copied += 1;
        }
        self.report.copied.insert(table, copied);
        Ok(())
    }
}

/// A group record's columns, its flags as 0 or 1 as SQLite stores them.
fn record_columns<R: Serialize>(record: &R) -> Result<Row> {
    let Value::Object(mut map) = serde_json::to_value(record).map_err(engine_error)? else {
        return Err(StoreError::Engine("a record is not an object".to_string()));
    };
    for value in map.values_mut() {
        if let Value::Bool(flag) = value {
            *value = Value::from(i64::from(*flag));
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests;
