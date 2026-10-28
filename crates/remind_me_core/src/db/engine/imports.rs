//! Import bookkeeping on the engine core (ADR-0023, core PR 4a, built
//! dark): `chat_imports`, `dbs_imports` and `mempalace_imports`, and the
//! reads that find what an import wrote. [`crate::db::imports`] calls these
//! when its store carries the core.
//!
//! The SQL's `LIKE` is kept as SQLite runs it: `%` and `_` are wildcards
//! even inside the caller's prefix, and case folds for ASCII only.

use super::core::{Change, CoreTables};
use super::memories::{self, ByDoc, MemoryRecord, MemoryRow};
use super::{core_ref, engine_id, ensure_same_id, pair_engine_id, EngineTables};
use crate::db::imports::{DbsKey, DbsTracked};
use crate::db::{Result, StoreError};
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use uuid::Uuid;

/// Index marker: a chat import by its content hash.
pub struct ByHash;
/// Index marker: a tracking row by the memory it recorded.
pub struct ByMemory;
/// Slot marker: unused, always 0.
pub struct Slot;

pub(crate) type ChatImportTable = GenericMmapStore<ChatImportRecord, ByHash, Slot>;
pub(crate) type DbsImportTable = GenericMmapStore<DbsImportRecord, ByMemory, Slot>;
pub(crate) type MempalaceImportTable = GenericMmapStore<MempalaceImportRecord, ByMemory, Slot>;

/// One `chat_imports` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatImportRecord {
    engine_id: Uuid,
    import_id: String,
    filename: String,
    hash: String,
    imported_at: String,
    stats: String,
    slot: i64,
}

/// One `dbs_imports` row, keyed by its (source, external id) pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DbsImportRecord {
    engine_id: Uuid,
    dbs_source: String,
    external_id: String,
    memory_id: String,
    content_hash: String,
    imported_at: String,
    slot: i64,
}

/// One `mempalace_imports` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MempalaceImportRecord {
    engine_id: Uuid,
    drawer_id: String,
    memory_id: String,
    imported_at: String,
    slot: i64,
}

/// The engine traits for a tracking record: its id, schema tag, the field
/// its index reads, and an unused slot.
macro_rules! tracking_record {
    ($record:ty, $tag:literal, $marker:ty, $field:ident) => {
        impl Record for $record {
            type Id = Uuid;
            fn id(&self) -> Uuid {
                self.engine_id
            }
        }

        impl SchemaTag for $record {
            const SCHEMA_TAG: &'static str = $tag;
        }

        impl IndexedField<$marker> for $record {
            type IndexValue = String;
            fn indexed_value(&self) -> &String {
                &self.$field
            }
        }

        impl ScannableField<Slot> for $record {
            type ScanValue = i64;
            fn scannable_value(&self) -> i64 {
                self.slot
            }
            fn set_scannable_value(&mut self, value: i64) {
                self.slot = value;
            }
        }
    };
}

tracking_record!(
    ChatImportRecord,
    "rusty_remind_me::node::ChatImportRecord@1",
    ByHash,
    hash
);
tracking_record!(
    DbsImportRecord,
    "rusty_remind_me::node::DbsImportRecord@1",
    ByMemory,
    memory_id
);
tracking_record!(
    MempalaceImportRecord,
    "rusty_remind_me::node::MempalaceImportRecord@1",
    ByMemory,
    memory_id
);

/// SQLite's `value LIKE pattern` with no `ESCAPE`: `%` matches any run of
/// characters, `_` any one, and letters match regardless of ASCII case.
pub(crate) fn like(value: &str, pattern: &str) -> bool {
    let value: Vec<char> = value.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut v, mut p) = (0, 0);
    // Where the last `%` was, and the value position it is retried from.
    let mut backtrack: Option<(usize, usize)> = None;
    while v < value.len() {
        match pattern.get(p) {
            Some('%') => {
                backtrack = Some((p, v));
                p += 1;
            }
            Some(&c) if c == '_' || c.eq_ignore_ascii_case(&value[v]) => {
                p += 1;
                v += 1;
            }
            _ => {
                let Some((star, from)) = backtrack else {
                    return false;
                };
                backtrack = Some((star, from + 1));
                p = star + 1;
                v = from + 1;
            }
        }
    }
    pattern[p..].iter().all(|&c| c == '%')
}

fn starts_like(value: &str, prefix: &str) -> bool {
    like(value, &format!("{prefix}%"))
}

fn live_row(core: &CoreTables, memory_id: &str) -> Option<MemoryRow> {
    memories::row(core, memory_id).filter(|r| r.deleted_at.is_none())
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

// --- chat imports -----------------------------------------------------------

fn chat_imports(core: &CoreTables) -> Vec<ChatImportRecord> {
    core.chat_imports
        .all_ids()
        .into_iter()
        .filter_map(|id| core.chat_imports.get(id))
        .collect()
}

fn chat_import(core: &CoreTables, import_id: &str) -> Option<ChatImportRecord> {
    core.chat_imports
        .get(engine_id(import_id))
        .filter(|r| r.import_id == import_id)
}

/// Record a chat import: a plain `INSERT`, so an id already recorded is
/// refused.
pub(crate) fn record_chat(
    tables: &mut EngineTables,
    import_id: &str,
    filename: &str,
    hash: &str,
    imported_at: &str,
    stats_json: &str,
) -> Result<()> {
    let id = engine_id(import_id);
    if let Some(stored) = core_ref(tables)?.chat_imports.get(id) {
        ensure_same_id(&stored.import_id, import_id)?;
        return Err(StoreError::Engine(format!(
            "chat import {import_id:?} is already recorded"
        )));
    }
    let record = ChatImportRecord {
        engine_id: id,
        import_id: import_id.to_string(),
        filename: filename.to_string(),
        hash: hash.to_string(),
        imported_at: imported_at.to_string(),
        stats: stats_json.to_string(),
        slot: 0,
    };
    tables.commit(vec![Change::ChatImport(id, Some(Box::new(record)))])
}

/// The earliest chat import recorded for `hash` (ties by id).
pub(crate) fn chat_import_with_hash(tables: &EngineTables, hash: &str) -> Result<Option<String>> {
    let core = core_ref(tables)?;
    Ok(
        FilterEq::<ChatImportRecord, ByHash>::filter_eq(&core.chat_imports, &hash.to_string())
            .into_iter()
            .filter_map(|id| core.chat_imports.get(id))
            .filter(|r| r.hash == hash)
            .min_by(|a, b| (&a.imported_at, &a.import_id).cmp(&(&b.imported_at, &b.import_id)))
            .map(|r| r.import_id),
    )
}

/// Live memories whose `doc_id` is `doc_id`.
fn live_of_doc(core: &CoreTables, doc_id: &str) -> Vec<String> {
    FilterEq::<MemoryRecord, ByDoc>::filter_eq(&core.memories, &doc_id.to_string())
        .into_iter()
        .filter_map(|id| core.memories.get(id))
        .map(|record| record.row)
        .filter(|r| r.doc_id.as_deref() == Some(doc_id) && r.deleted_at.is_none())
        .map(|r| r.id)
        .collect()
}

pub(crate) fn live_chat_memories(
    tables: &EngineTables,
    import_id: Option<&str>,
) -> Result<Vec<String>> {
    let core = core_ref(tables)?;
    let ids = match import_id {
        Some(id) => live_of_doc(core, id),
        None => chat_imports(core)
            .into_iter()
            .flat_map(|r| live_of_doc(core, &r.import_id))
            .collect(),
    };
    Ok(sorted(ids))
}

pub(crate) fn chat_imports_with_nothing_left(
    tables: &EngineTables,
    import_ids: &[String],
) -> Result<Vec<String>> {
    let core = core_ref(tables)?;
    let wanted: BTreeSet<&str> = import_ids.iter().map(String::as_str).collect();
    Ok(wanted
        .into_iter()
        .filter_map(|id| chat_import(core, id))
        .filter(|r| live_of_doc(core, &r.import_id).is_empty())
        .map(|r| r.import_id)
        .collect())
}

pub(crate) fn forget_chat_imports_with_nothing_left(
    tables: &mut EngineTables,
    import_ids: &[String],
) -> Result<usize> {
    let doomed = chat_imports_with_nothing_left(tables, import_ids)?;
    let count = doomed.len();
    tables.commit(
        doomed
            .iter()
            .map(|id| Change::ChatImport(engine_id(id), None))
            .collect(),
    )?;
    Ok(count)
}

pub(crate) fn chat_import_count(tables: &EngineTables) -> Result<i64> {
    let count = core_ref(tables)?.chat_imports.all_ids().len();
    Ok(i64::try_from(count).unwrap_or(i64::MAX))
}

// --- dbs imports ------------------------------------------------------------

fn dbs_row(core: &CoreTables, source: &str, external_id: &str) -> Option<DbsImportRecord> {
    core.dbs_imports
        .get(pair_engine_id(source, external_id))
        .filter(|r| r.dbs_source == source && r.external_id == external_id)
}

pub(crate) fn dbs_tracked(
    tables: &EngineTables,
    source: &str,
    external_ids: &[&str],
) -> Result<HashMap<DbsKey, DbsTracked>> {
    let core = core_ref(tables)?;
    Ok(external_ids
        .iter()
        .filter_map(|external_id| dbs_row(core, source, external_id))
        .map(|r| {
            (
                (r.dbs_source, r.external_id),
                DbsTracked {
                    memory_id: r.memory_id,
                    content_hash: r.content_hash,
                },
            )
        })
        .collect())
}

/// Record a dbs item: the SQL's upsert on (source, external id).
pub(crate) fn record_dbs(
    tables: &mut EngineTables,
    source: &str,
    external_id: &str,
    memory_id: &str,
    content_hash: &str,
    imported_at: &str,
) -> Result<()> {
    let id = pair_engine_id(source, external_id);
    let record = DbsImportRecord {
        engine_id: id,
        dbs_source: source.to_string(),
        external_id: external_id.to_string(),
        memory_id: memory_id.to_string(),
        content_hash: content_hash.to_string(),
        imported_at: imported_at.to_string(),
        slot: 0,
    };
    tables.commit(vec![Change::DbsImport(id, Some(Box::new(record)))])
}

pub(crate) fn live_dbs_memories(
    tables: &EngineTables,
    source_prefix: Option<&str>,
) -> Result<Vec<String>> {
    let core = core_ref(tables)?;
    let ids = core
        .dbs_imports
        .all_ids()
        .into_iter()
        .filter_map(|id| core.dbs_imports.get(id))
        .filter(|r| source_prefix.is_none_or(|p| starts_like(&r.dbs_source, p)))
        .filter(|r| live_row(core, &r.memory_id).is_some())
        .map(|r| r.memory_id)
        .collect();
    Ok(sorted(ids))
}

// --- mempalace imports ------------------------------------------------------

fn drawer(core: &CoreTables, drawer_id: &str) -> Option<MempalaceImportRecord> {
    core.mempalace_imports
        .get(engine_id(drawer_id))
        .filter(|r| r.drawer_id == drawer_id)
}

pub(crate) fn imported_drawers(tables: &EngineTables, drawer_ids: &[&str]) -> Result<Vec<String>> {
    let core = core_ref(tables)?;
    let wanted: BTreeSet<&str> = drawer_ids.iter().copied().collect();
    Ok(wanted
        .into_iter()
        .filter_map(|id| drawer(core, id))
        .map(|r| r.drawer_id)
        .collect())
}

/// Record a drawer unless it is already recorded: `INSERT OR IGNORE`.
pub(crate) fn record_mempalace(
    tables: &mut EngineTables,
    drawer_id: &str,
    memory_id: &str,
    imported_at: &str,
) -> Result<()> {
    let id = engine_id(drawer_id);
    if let Some(stored) = core_ref(tables)?.mempalace_imports.get(id) {
        return ensure_same_id(&stored.drawer_id, drawer_id);
    }
    let record = MempalaceImportRecord {
        engine_id: id,
        drawer_id: drawer_id.to_string(),
        memory_id: memory_id.to_string(),
        imported_at: imported_at.to_string(),
        slot: 0,
    };
    tables.commit(vec![Change::MempalaceImport(id, Some(record))])
}

pub(crate) fn live_tracked_mempalace_memories(
    tables: &EngineTables,
    drawer_prefix: Option<&str>,
) -> Result<Vec<String>> {
    let core = core_ref(tables)?;
    let ids = core
        .mempalace_imports
        .all_ids()
        .into_iter()
        .filter_map(|id| core.mempalace_imports.get(id))
        .filter(|r| drawer_prefix.is_none_or(|p| starts_like(&r.drawer_id, p)))
        .filter(|r| live_row(core, &r.memory_id).is_some())
        .map(|r| r.memory_id)
        .collect();
    Ok(sorted(ids))
}

/// `json_extract(metadata, '$.<key>')` as text for `LIKE`: a string as
/// itself, a number as its digits, a boolean as 1 or 0, a container as its
/// JSON; `None` for null or a missing key. Malformed JSON fails, as
/// `json_extract` does.
fn extracted_text(metadata: &str, key: &str) -> Result<Option<String>> {
    let parsed: Value = serde_json::from_str(metadata)
        .map_err(|e| StoreError::Engine(format!("malformed JSON: {e}")))?;
    Ok(match parsed.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Bool(b)) => Some(if *b { "1" } else { "0" }.to_string()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(other) => Some(other.to_string()),
    })
}

pub(crate) fn live_mempalace_shaped_memories(
    tables: &EngineTables,
    drawer_prefix: Option<&str>,
) -> Result<Vec<String>> {
    let core = core_ref(tables)?;
    let mut ids = Vec::new();
    for row in memories::rows(core) {
        if row.deleted_at.is_some()
            || !(row.source == "mempalace_import" || starts_like(&row.source, "mempalace:"))
        {
            continue;
        }
        if let Some(prefix) = drawer_prefix {
            let drawer = extracted_text(&row.metadata, "mempalace_drawer_id")?;
            if !drawer.is_some_and(|d| starts_like(&d, prefix)) {
                continue;
            }
        }
        ids.push(row.id);
    }
    Ok(sorted(ids))
}

// --- undo -------------------------------------------------------------------

pub(crate) fn doc_ids_of(tables: &EngineTables, memory_ids: &[String]) -> Result<Vec<String>> {
    let core = core_ref(tables)?;
    let docs: BTreeSet<String> = memory_ids
        .iter()
        .filter_map(|id| memories::row(core, id))
        .filter_map(|r| r.doc_id)
        .collect();
    Ok(docs.into_iter().collect())
}

pub(crate) fn forget_dbs(tables: &mut EngineTables, memory_ids: &[String]) -> Result<usize> {
    let core = core_ref(tables)?;
    let doomed = tracked_by(memory_ids, |memory_id| {
        FilterEq::<DbsImportRecord, ByMemory>::filter_eq(&core.dbs_imports, memory_id)
            .into_iter()
            .filter_map(|id| core.dbs_imports.get(id))
            .filter(|r| &r.memory_id == memory_id)
            .map(|r| r.engine_id)
            .collect()
    });
    let count = doomed.len();
    tables.commit(
        doomed
            .into_iter()
            .map(|id| Change::DbsImport(id, None))
            .collect(),
    )?;
    Ok(count)
}

pub(crate) fn forget_mempalace(tables: &mut EngineTables, memory_ids: &[String]) -> Result<usize> {
    let core = core_ref(tables)?;
    let doomed = tracked_by(memory_ids, |memory_id| {
        FilterEq::<MempalaceImportRecord, ByMemory>::filter_eq(&core.mempalace_imports, memory_id)
            .into_iter()
            .filter_map(|id| core.mempalace_imports.get(id))
            .filter(|r| &r.memory_id == memory_id)
            .map(|r| r.engine_id)
            .collect()
    });
    let count = doomed.len();
    tables.commit(
        doomed
            .into_iter()
            .map(|id| Change::MempalaceImport(id, None))
            .collect(),
    )?;
    Ok(count)
}

/// The engine ids `rows_of` finds for each of `memory_ids`, each once: the
/// SQL's `WHERE memory_id IN (...)`.
fn tracked_by(memory_ids: &[String], rows_of: impl Fn(&String) -> Vec<Uuid>) -> Vec<Uuid> {
    let unique: BTreeSet<&String> = memory_ids.iter().collect();
    let mut seen = HashSet::new();
    unique
        .into_iter()
        .flat_map(rows_of)
        .filter(|id| seen.insert(*id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::like;

    #[test]
    fn like_matches_as_sqlite_does() {
        assert!(like("Mempalace:x", "mempalace:%"));
        assert!(like("abc", "a_c"));
        assert!(like("a_c", "a_c"));
        assert!(like("axyzc", "a%c"));
        assert!(like("", "%"));
        assert!(like("abc", "%%c"));
        assert!(!like("ab", "a_c"));
        assert!(!like("abcd", "a%c"));
        // Only ASCII folds.
        assert!(!like("Ä", "ä"));
        assert!(like("ä", "_"));
    }
}
