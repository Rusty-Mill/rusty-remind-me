//! Test support: raw reads and writes of stored rows that work on every
//! backend (ADR-0023, core PR 5).
//!
//! Tests used to seed and inspect memories with SQL beside the repositories.
//! Once the memories core is on, that SQL reaches tables nothing reads, so
//! the same raw access lives here instead: one column of one memory, read
//! or overwritten as an `UPDATE` would (no outbox entry, no revision), and
//! row counts. On SQLite these are the statements the tests ran; on the
//! core they read and write the stored row.
//!
//! Not part of the node's API: it exists for this crate's tests and the
//! integration tests beside it.

#[cfg(feature = "engine-store")]
use crate::db::engine::{self, EngineTables};
use crate::db::{Result, Store, StoreError};
#[cfg(feature = "engine-store")]
use parking_lot::Mutex;
use rusqlite::types::Value as SqlValue;
use rusqlite::OptionalExtension;
pub use serde_json::Value;

/// The columns of `memories`, as the schema names them.
const MEMORY_COLUMNS: &[&str] = &[
    "id",
    "content",
    "category",
    "tags",
    "source",
    "metadata",
    "created_at",
    "updated_at",
    "capture_id",
    "subject",
    "predicate",
    "object",
    "superseded_by",
    "decay_rate",
    "vitality",
    "base_weight",
    "access_count",
    "accessed_at",
    "doc_id",
    "chunk_index",
    "remind_at",
    "sensitive",
    "memory_type",
    "status",
    "node_id",
    "client",
    "source_capture_id",
    "deleted_at",
];

/// A table whose rows [`count`] can count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Memories,
    SyncOutbox,
    SyncSends,
    ReminderDeliveries,
    MemoryFeedback,
    Entities,
    MemoryEntities,
    EntityRelations,
    MemoryAssociations,
    Promotions,
    VecChunks,
    ChatImports,
    DbsImports,
    MempalaceImports,
}

impl Table {
    fn name(self) -> &'static str {
        match self {
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
}

fn memory_column_name(column: &str) -> Result<&str> {
    MEMORY_COLUMNS
        .iter()
        .find(|c| **c == column)
        .copied()
        .ok_or_else(|| StoreError::Invalid(format!("memories has no column {column:?}")))
}

#[cfg(feature = "engine-store")]
fn core<'s>(store: &'s Store<'_>) -> Option<&'s Mutex<EngineTables>> {
    store.core()
}

/// Memory `id`'s `column` as SQLite returns it, as JSON: text as a string,
/// an integer or real as a number (a flag as 0 or 1), NULL as null. `None`
/// when there is no such memory.
///
/// # Errors
///
/// [`StoreError::Invalid`] for a column `memories` does not have, or the
/// store's own error.
pub fn memory_column(store: &Store<'_>, id: &str, column: &str) -> Result<Option<Value>> {
    let column = memory_column_name(column)?;
    #[cfg(feature = "engine-store")]
    if let Some(core) = core(store) {
        return engine::testing::memory_column(&core.lock(), id, column);
    }
    let value: Option<SqlValue> = store
        .conn()
        .query_row(
            &format!("SELECT {column} FROM memories WHERE id = ?"),
            [id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(value.map(|v| match v {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(i) => Value::from(i),
        SqlValue::Real(f) => Value::from(f),
        SqlValue::Text(s) => Value::String(s),
        SqlValue::Blob(b) => Value::from(b),
    }))
}

/// [`memory_column`] as text: `None` for NULL or no such memory.
pub fn memory_text(store: &Store<'_>, id: &str, column: &str) -> Result<Option<String>> {
    Ok(memory_column(store, id, column)?.and_then(|v| match v {
        Value::String(s) => Some(s),
        Value::Null => None,
        other => Some(other.to_string()),
    }))
}

/// [`memory_column`] as an integer: `None` for NULL or no such memory.
pub fn memory_i64(store: &Store<'_>, id: &str, column: &str) -> Result<Option<i64>> {
    Ok(memory_column(store, id, column)?.and_then(|v| v.as_i64()))
}

/// [`memory_column`] as a real: `None` for NULL or no such memory.
pub fn memory_f64(store: &Store<'_>, id: &str, column: &str) -> Result<Option<f64>> {
    Ok(memory_column(store, id, column)?.and_then(|v| v.as_f64()))
}

/// Overwrite memory `id`'s `column` with `value`, as `UPDATE memories SET
/// column = ? WHERE id = ?` does: no outbox entry, no revision. Returns how
/// many rows changed (0 when there is no such memory).
///
/// # Errors
///
/// [`StoreError::Invalid`] for a column `memories` does not have or a value
/// it cannot hold, or the store's own error.
pub fn set_memory_column(
    store: &Store<'_>,
    id: &str,
    column: &str,
    value: impl Into<Value>,
) -> Result<usize> {
    let column = memory_column_name(column)?;
    let value = value.into();
    #[cfg(feature = "engine-store")]
    if let Some(core) = core(store) {
        return engine::testing::set_memory_column(&mut core.lock(), id, column, value);
    }
    let value = match value {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or_default()),
        },
        Value::String(s) => SqlValue::Text(s),
        other => SqlValue::Text(other.to_string()),
    };
    Ok(store.conn().execute(
        &format!("UPDATE memories SET {column} = ? WHERE id = ?"),
        rusqlite::params![value, id],
    )?)
}

/// Every memory id, deleted or not, sorted.
pub fn memory_ids(store: &Store<'_>) -> Result<Vec<String>> {
    #[cfg(feature = "engine-store")]
    if let Some(core) = core(store) {
        return engine::testing::memory_ids(&core.lock());
    }
    let mut stmt = store
        .conn()
        .prepare("SELECT id FROM memories ORDER BY id")?;
    let ids = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Move memory `from` to id `to`, every other column kept, as `UPDATE
/// memories SET id = ? WHERE id = ?` does (with `memory_tags` following):
/// no outbox entry, no revision. Stands in for a row another process wrote
/// under an id of its own shape. Returns how many rows moved (0 when there
/// is no such memory).
///
/// # Errors
///
/// [`StoreError::Invalid`] when `to` is taken or the move would leave tag
/// rows behind, or the store's own error.
pub fn relabel_memory(store: &Store<'_>, from: &str, to: &str) -> Result<usize> {
    #[cfg(feature = "engine-store")]
    if let Some(core) = core(store) {
        return engine::testing::relabel_memory(&mut core.lock(), from, to);
    }
    let conn = store.conn();
    // `memory_tags.memory_id` references `memories.id`, so whichever update
    // lands first orphans the other: the check is off across the pair.
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    let moved = conn.execute("UPDATE memories SET id = ?1 WHERE id = ?2", [to, from]);
    let tags = conn.execute(
        "UPDATE memory_tags SET memory_id = ?1 WHERE memory_id = ?2",
        [to, from],
    );
    conn.execute_batch("PRAGMA foreign_keys = ON")?;
    let (moved, _) = (moved?, tags?);
    let orphans: i64 = conn.query_row(
        "SELECT count(*) FROM memory_tags t
          LEFT JOIN memories m ON m.id = t.memory_id
          WHERE m.id IS NULL",
        [],
        |r| r.get(0),
    )?;
    if orphans != 0 {
        return Err(StoreError::Invalid(format!(
            "relabel left {orphans} orphaned tag rows"
        )));
    }
    Ok(moved)
}

/// How many rows `table` holds.
pub fn count(store: &Store<'_>, table: Table) -> Result<i64> {
    #[cfg(feature = "engine-store")]
    if let Some(core) = core(store) {
        return engine::testing::count(&core.lock(), table);
    }
    Ok(store
        .conn()
        .query_row(&format!("SELECT count(*) FROM {}", table.name()), [], |r| {
            r.get(0)
        })?)
}

/// The raw `query` of every feedback event logged for `memory_id`, oldest
/// first (ties by id), as `SELECT query FROM memory_feedback` returns it.
/// [`crate::db::feedback::Feedback::events`] carries only the tokens.
pub fn feedback_queries(store: &Store<'_>, memory_id: &str) -> Result<Vec<String>> {
    #[cfg(feature = "engine-store")]
    if let Some(core) = core(store) {
        return engine::testing::feedback_queries(&core.lock(), memory_id);
    }
    let mut stmt = store
        .conn()
        .prepare("SELECT query FROM memory_feedback WHERE memory_id = ? ORDER BY created_at, id")?;
    let queries = stmt
        .query_map([memory_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(queries)
}

/// The weight of the `memory_associations` row stored under exactly the
/// pair `(a, b)`, in that order: `SELECT weight FROM memory_associations
/// WHERE memory_id_a = ? AND memory_id_b = ?`. `None` when there is none.
/// [`crate::db::related::Related::co_retrieved`] reads only pairs whose
/// other memory is stored and live.
pub fn association_weight(store: &Store<'_>, a: &str, b: &str) -> Result<Option<i64>> {
    #[cfg(feature = "engine-store")]
    if let Some(core) = core(store) {
        return engine::testing::association_weight(&core.lock(), a, b);
    }
    Ok(store
        .conn()
        .query_row(
            "SELECT weight FROM memory_associations WHERE memory_id_a = ? AND memory_id_b = ?",
            [a, b],
            |r| r.get(0),
        )
        .optional()?)
}

/// One `sync_outbox` row, as [`outbox_rows`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub id: i64,
    pub memory_id: String,
    pub operation: String,
    pub payload: String,
    pub created_at: String,
    pub sent_at: String,
}

/// Every outbox row, oldest id first: `SELECT * FROM sync_outbox ORDER BY id`.
pub fn outbox_rows(store: &Store<'_>) -> Result<Vec<OutboxRow>> {
    #[cfg(feature = "engine-store")]
    if let Some(core) = core(store) {
        return engine::testing::outbox_rows(&core.lock());
    }
    let mut stmt = store.conn().prepare(
        "SELECT id, memory_id, operation, payload, created_at, sent_at
           FROM sync_outbox ORDER BY id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(OutboxRow {
                id: r.get(0)?,
                memory_id: r.get(1)?,
                operation: r.get(2)?,
                payload: r.get(3)?,
                created_at: r.get(4)?,
                sent_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Overwrite outbox row `id`'s `column` (`created_at` or `sent_at`) with
/// `value`, as `UPDATE sync_outbox SET column = ? WHERE id = ?` does.
/// Returns how many rows changed.
///
/// # Errors
///
/// [`StoreError::Invalid`] for any other column, or the store's own error.
pub fn set_outbox_column(store: &Store<'_>, id: i64, column: &str, value: &str) -> Result<usize> {
    if !matches!(column, "created_at" | "sent_at") {
        return Err(StoreError::Invalid(format!(
            "sync_outbox.{column} cannot be set here"
        )));
    }
    #[cfg(feature = "engine-store")]
    if let Some(core) = core(store) {
        return engine::testing::set_outbox_column(&mut core.lock(), id, column, value);
    }
    Ok(store.conn().execute(
        &format!("UPDATE sync_outbox SET {column} = ? WHERE id = ?"),
        rusqlite::params![value, id],
    )?)
}

/// Every send marker as `(remote, outbox id, sent at)`, sorted.
pub fn sends(store: &Store<'_>) -> Result<Vec<(String, i64, String)>> {
    #[cfg(feature = "engine-store")]
    if let Some(core) = core(store) {
        return engine::outbox::send_markers(&core.lock());
    }
    let mut stmt = store.conn().prepare(
        "SELECT remote_id, outbox_id, sent_at FROM sync_sends ORDER BY remote_id, outbox_id, sent_at",
    )?;
    let sends = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(sends)
}

/// Queue a raw outbox entry for `key`, created at `created_at`, as an
/// `INSERT INTO sync_outbox` does. Returns its id.
pub fn queue_outbox(
    store: &Store<'_>,
    key: &str,
    operation: &str,
    payload: &str,
    created_at: &str,
) -> Result<i64> {
    #[cfg(feature = "engine-store")]
    if let Some(core) = core(store) {
        return engine::outbox::queue_at(&mut core.lock(), key, operation, payload, created_at);
    }
    store.conn().execute(
        "INSERT INTO sync_outbox (memory_id, operation, payload, created_at) VALUES (?, ?, ?, ?)",
        rusqlite::params![key, operation, payload, created_at],
    )?;
    Ok(store.conn().last_insert_rowid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::on_each_core_backend;

    const NOW: &str = "2026-09-27T00:00:00+00:00";

    fn exercise(db: &crate::db::Database) -> Vec<String> {
        let store = db.store();
        Memories::new(&store)
            .insert(&NewMemory {
                sensitive: true,
                ..NewMemory::new("b", "x", NOW)
            })
            .unwrap();
        Memories::new(&store)
            .insert(&NewMemory::new("a", "y", NOW))
            .unwrap();
        let mut seen = vec![
            format!("{:?}", memory_column(&store, "b", "sensitive").unwrap()),
            format!("{:?}", memory_column(&store, "b", "superseded_by").unwrap()),
            format!("{:?}", memory_column(&store, "b", "vitality").unwrap()),
            format!("{:?}", memory_column(&store, "missing", "content").unwrap()),
            format!("{:?}", memory_column(&store, "b", "nope").is_err()),
            format!(
                "{}",
                set_memory_column(&store, "b", "sensitive", 0).unwrap()
            ),
            format!(
                "{}",
                set_memory_column(&store, "b", "superseded_by", "a").unwrap()
            ),
            format!(
                "{}",
                set_memory_column(&store, "b", "access_count", 7).unwrap()
            ),
            format!(
                "{}",
                set_memory_column(&store, "b", "remind_at", Value::Null).unwrap()
            ),
            format!(
                "{}",
                set_memory_column(&store, "gone", "content", "z").unwrap()
            ),
        ];
        seen.push(format!(
            "{:?} {:?} {:?} {:?}",
            memory_i64(&store, "b", "sensitive").unwrap(),
            memory_text(&store, "b", "superseded_by").unwrap(),
            memory_i64(&store, "b", "access_count").unwrap(),
            memory_text(&store, "b", "remind_at").unwrap(),
        ));
        seen.push(format!("{:?}", memory_ids(&store).unwrap()));
        seen.push(format!("{}", count(&store, Table::Memories).unwrap()));
        seen.push(format!("{}", count(&store, Table::Entities).unwrap()));
        let queued = queue_outbox(&store, "a", "insert", "{}", NOW).unwrap();
        seen.push(format!(
            "{} {} {:?}",
            set_outbox_column(&store, queued, "sent_at", NOW).unwrap(),
            set_outbox_column(&store, queued + 1000, "created_at", NOW).unwrap(),
            set_outbox_column(&store, queued, "payload", "x").is_err(),
        ));
        seen.push(format!(
            "{:?}",
            outbox_rows(&store)
                .unwrap()
                .into_iter()
                .map(|r| (r.memory_id, r.operation, r.payload, r.created_at, r.sent_at))
                .collect::<Vec<_>>()
        ));
        let feedback = crate::db::feedback::Feedback::new(&store);
        for (id, query, at) in [("f2", "later", "2026-09-28"), ("f1", "first", NOW)] {
            let event = crate::db::feedback::FeedbackEvent {
                query_tokens: query.to_string(),
                signal: "helpful".to_string(),
                magnitude: 0.1,
            };
            feedback.log_event(id, "b", query, &event, at).unwrap();
        }
        seen.push(format!(
            "{:?} {:?}",
            feedback_queries(&store, "b").unwrap(),
            feedback_queries(&store, "a").unwrap(),
        ));
        let related = crate::db::related::Related::new(&store);
        related.bump_pair("a", "b", NOW, 10).unwrap();
        related.bump_pair("a", "b", NOW, 10).unwrap();
        seen.push(format!(
            "{:?} {:?} {:?}",
            association_weight(&store, "a", "b").unwrap(),
            association_weight(&store, "b", "a").unwrap(),
            association_weight(&store, "a", "z").unwrap(),
        ));
        seen.push(format!(
            "{} {} {:?} {:?} {:?}",
            relabel_memory(&store, "a", "c").unwrap(),
            relabel_memory(&store, "gone", "d").unwrap(),
            relabel_memory(&store, "c", "b").is_err(),
            memory_ids(&store).unwrap(),
            memory_text(&store, "c", "content").unwrap(),
        ));
        seen
    }

    #[test]
    fn raw_access_reads_and_writes_alike_on_every_backend() {
        let mut observed = Vec::new();
        on_each_core_backend(|db| observed.push(exercise(db)));
        let sqlite = &observed[0];
        assert_eq!(sqlite[0], "Some(Number(1))");
        for other in &observed[1..] {
            assert_eq!(other, sqlite);
        }
    }
}
