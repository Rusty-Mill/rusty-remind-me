//! Test support: raw reads and writes of stored rows.
//!
//! Tests used to seed and inspect memories with SQL beside the repositories.
//! The same raw access lives here instead: one column of one memory, read or
//! overwritten as an `UPDATE` would (no outbox entry, no revision), and row
//! counts. The column names and value shapes are those of the schema the
//! engine's records mirror (`db::legacy_sqlite::SCHEMA_VERSION`): text as a
//! string, a flag as 0 or 1, NULL as null.
//!
//! Not part of the node's API: it exists for this crate's tests and the
//! integration tests beside it.
//!
//! These helpers reach past the ports (ADR-0035) through [`TestRows`],
//! which each backend implements over its own rows (D42), so the whole
//! suite runs on every backend. A backend without it makes them refuse.

use crate::db::{Result, Store, StoreError};
pub use serde_json::Value;

/// The columns of `memories`, as the schema names them.
pub const MEMORY_COLUMNS: &[&str] = &[
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
    "project",
    "session_id",
    "git_remote",
    "git_branch",
    "git_sha",
    "cwd",
    "valid_from",
    "valid_until",
    "confidence",
    "verified_at",
    "outcome",
    "written_by",
    "capture_method",
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

fn memory_column_name(column: &str) -> Result<&str> {
    MEMORY_COLUMNS
        .iter()
        .find(|c| **c == column)
        .copied()
        .ok_or_else(|| StoreError::Invalid(format!("memories has no column {column:?}")))
}

/// The raw rows a backend keeps, for the helpers below (D42). Each method
/// is the raw statement named in its helper's doc: it reaches past the
/// ports, so it writes no outbox entry and no revision. A backend offers it
/// through [`crate::db::backend::Backend::test_rows`]; one that does not
/// makes every helper refuse.
///
/// Column names are checked before a call gets here.
#[doc(hidden)]
pub trait TestRows {
    /// See [`memory_column`].
    fn memory_column(&self, id: &str, column: &str) -> Result<Option<Value>>;
    /// See [`set_memory_column`].
    fn set_memory_column(&mut self, id: &str, column: &str, value: Value) -> Result<usize>;
    /// See [`relabel_memory`].
    fn relabel_memory(&mut self, from: &str, to: &str) -> Result<usize>;
    /// See [`memory_ids`].
    fn memory_ids(&self) -> Result<Vec<String>>;
    /// See [`count`].
    fn count(&self, table: Table) -> Result<i64>;
    /// See [`feedback_queries`].
    fn feedback_queries(&self, memory_id: &str) -> Result<Vec<String>>;
    /// See [`association_weight`].
    fn association_weight(&self, a: &str, b: &str) -> Result<Option<i64>>;
    /// See [`outbox_rows`].
    fn outbox_rows(&self) -> Result<Vec<OutboxRow>>;
    /// See [`set_outbox_column`]; `column` is `created_at` or `sent_at`.
    fn set_outbox_column(&mut self, id: i64, column: &str, value: &str) -> Result<usize>;
    /// See [`sends`].
    fn sends(&self) -> Result<Vec<(String, i64, String)>>;
    /// See [`queue_outbox`].
    fn queue_outbox(
        &mut self,
        key: &str,
        operation: &str,
        payload: &str,
        created_at: &str,
    ) -> Result<i64>;
}

/// Run `f` on the store's raw rows, or refuse when its backend has none.
fn with_rows<T>(store: &Store<'_>, f: impl FnOnce(&mut dyn TestRows) -> Result<T>) -> Result<T> {
    let mut backend = store.backend().lock()?;
    let rows = backend.test_rows().ok_or_else(|| {
        StoreError::Invalid("raw rows were asked of a store whose backend has none".into())
    })?;
    f(rows)
}

/// Memory `id`'s `column` as JSON: text as a string, an integer or real as
/// a number (a flag as 0 or 1), NULL as null. `None` when there is no such
/// memory.
///
/// # Errors
///
/// [`StoreError::Invalid`] for a column `memories` does not have, or the
/// store's own error.
pub fn memory_column(store: &Store<'_>, id: &str, column: &str) -> Result<Option<Value>> {
    let column = memory_column_name(column)?;
    with_rows(store, |rows| rows.memory_column(id, column))
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
/// column = ? WHERE id = ?` did: no outbox entry, no revision. Returns how
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
    with_rows(store, |rows| rows.set_memory_column(id, column, value))
}

/// Every memory id, deleted or not, sorted.
pub fn memory_ids(store: &Store<'_>) -> Result<Vec<String>> {
    with_rows(store, |rows| rows.memory_ids())
}

/// Move memory `from` to id `to`, every other column kept, as `UPDATE
/// memories SET id = ? WHERE id = ?` did: no outbox entry, no revision.
/// Stands in for a row another process wrote under an id of its own shape.
/// Returns how many rows moved (0 when there is no such memory).
///
/// # Errors
///
/// [`StoreError::Invalid`] when `to` is taken, or the store's own error.
pub fn relabel_memory(store: &Store<'_>, from: &str, to: &str) -> Result<usize> {
    with_rows(store, |rows| rows.relabel_memory(from, to))
}

/// How many rows `table` holds.
pub fn count(store: &Store<'_>, table: Table) -> Result<i64> {
    with_rows(store, |rows| rows.count(table))
}

/// The raw `query` of every feedback event logged for `memory_id`, oldest
/// first (ties by id). [`crate::db::feedback::Feedback::events`] carries
/// only the tokens.
pub fn feedback_queries(store: &Store<'_>, memory_id: &str) -> Result<Vec<String>> {
    with_rows(store, |rows| rows.feedback_queries(memory_id))
}

/// The weight of the `memory_associations` row stored under exactly the
/// pair `(a, b)`, in that order. `None` when there is none.
/// [`crate::db::related::Related::co_retrieved`] reads only pairs whose
/// other memory is stored and live.
pub fn association_weight(store: &Store<'_>, a: &str, b: &str) -> Result<Option<i64>> {
    with_rows(store, |rows| rows.association_weight(a, b))
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

/// Mark the store as one that synced before multi-node sync was removed
/// (card #746): its writes queue outbox rows and its deletes leave
/// tombstones, as such a store keeps doing.
pub fn mark_store_syncing(store: &Store<'_>) -> Result<()> {
    crate::db::sync_state::SyncState::new(store).set_flag("sync_enabled", "1")
}

/// Every outbox row, oldest id first.
pub fn outbox_rows(store: &Store<'_>) -> Result<Vec<OutboxRow>> {
    with_rows(store, |rows| rows.outbox_rows())
}

/// Overwrite outbox row `id`'s `column` (`created_at` or `sent_at`) with
/// `value`. Returns how many rows changed.
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
    with_rows(store, |rows| rows.set_outbox_column(id, column, value))
}

/// Every send marker as `(remote, outbox id, sent at)`, sorted.
pub fn sends(store: &Store<'_>) -> Result<Vec<(String, i64, String)>> {
    with_rows(store, |rows| rows.sends())
}

/// Queue a raw outbox entry for `key`, created at `created_at`, as an
/// `INSERT INTO sync_outbox` did. Returns its id.
pub fn queue_outbox(
    store: &Store<'_>,
    key: &str,
    operation: &str,
    payload: &str,
    created_at: &str,
) -> Result<i64> {
    with_rows(store, |rows| {
        rows.queue_outbox(key, operation, payload, created_at)
    })
}

/// Make this thread see every MCP tool, as before the default profile became
/// `core` (ADR-0030 step 2). For a test that exercises tools outside `core`
/// and is not about profiles: it sets a thread-local session, so it neither
/// reads nor writes the process environment and cannot race another test.
pub fn use_full_tool_surface() {
    use crate::daemon::session::{enter, Session};
    use crate::tool_profiles::TOOL_PROFILE_ENV;
    enter(Session {
        vars: [(TOOL_PROFILE_ENV.to_string(), "full".to_string())].into(),
        handshake_client: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-27T00:00:00+00:00";

    #[test]
    fn raw_access_reads_and_writes_engine_rows() {
        raw_access_reads_and_writes_stored_rows(&Database::open_in_memory().unwrap());
    }

    #[test]
    fn raw_access_reads_and_writes_sqlite_rows() {
        let sqlite = crate::db::sqlite::SqliteTables::open_in_memory().unwrap();
        raw_access_reads_and_writes_stored_rows(
            &Database::open_backend(Box::new(sqlite), None).unwrap(),
        );
    }

    /// The same raw reads and writes on any backend.
    fn raw_access_reads_and_writes_stored_rows(db: &Database) {
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
        assert_eq!(
            memory_column(&store, "b", "sensitive").unwrap(),
            Some(Value::from(1))
        );
        assert_eq!(
            memory_column(&store, "b", "superseded_by").unwrap(),
            Some(Value::Null)
        );
        assert_eq!(
            memory_column(&store, "b", "vitality").unwrap(),
            Some(Value::from(1.0))
        );
        assert_eq!(memory_column(&store, "missing", "content").unwrap(), None);
        assert!(memory_column(&store, "b", "nope").is_err());
        assert_eq!(set_memory_column(&store, "b", "sensitive", 0).unwrap(), 1);
        assert_eq!(
            set_memory_column(&store, "b", "superseded_by", "a").unwrap(),
            1
        );
        assert_eq!(
            set_memory_column(&store, "b", "access_count", 7).unwrap(),
            1
        );
        assert_eq!(
            set_memory_column(&store, "b", "remind_at", Value::Null).unwrap(),
            1
        );
        assert_eq!(
            set_memory_column(&store, "gone", "content", "z").unwrap(),
            0
        );
        assert_eq!(memory_i64(&store, "b", "sensitive").unwrap(), Some(0));
        assert_eq!(
            memory_text(&store, "b", "superseded_by")
                .unwrap()
                .as_deref(),
            Some("a")
        );
        assert_eq!(memory_i64(&store, "b", "access_count").unwrap(), Some(7));
        assert_eq!(memory_text(&store, "b", "remind_at").unwrap(), None);
        assert_eq!(memory_f64(&store, "b", "vitality").unwrap(), Some(1.0));
        assert_eq!(memory_ids(&store).unwrap(), ["a", "b"]);
        assert_eq!(count(&store, Table::Memories).unwrap(), 2);
        assert_eq!(count(&store, Table::Entities).unwrap(), 0);

        let queued = queue_outbox(&store, "a", "insert", "{}", NOW).unwrap();
        assert_eq!(
            set_outbox_column(&store, queued, "sent_at", NOW).unwrap(),
            1
        );
        assert_eq!(
            set_outbox_column(&store, queued + 1000, "created_at", NOW).unwrap(),
            0
        );
        assert!(set_outbox_column(&store, queued, "payload", "x").is_err());
        let rows: Vec<(String, String, String, String, String)> = outbox_rows(&store)
            .unwrap()
            .into_iter()
            .map(|r| (r.memory_id, r.operation, r.payload, r.created_at, r.sent_at))
            .collect();
        assert_eq!(
            rows,
            [(
                "a".to_string(),
                "insert".to_string(),
                "{}".to_string(),
                NOW.to_string(),
                NOW.to_string()
            )]
        );

        let feedback = crate::db::feedback::Feedback::new(&store);
        for (id, query, at) in [("f2", "later", "2026-09-28"), ("f1", "first", NOW)] {
            let event = crate::db::feedback::FeedbackEvent {
                query_tokens: query.to_string(),
                signal: "helpful".to_string(),
                magnitude: 0.1,
            };
            feedback.log_event(id, "b", query, &event, at).unwrap();
        }
        assert_eq!(feedback_queries(&store, "b").unwrap(), ["first", "later"]);
        assert!(feedback_queries(&store, "a").unwrap().is_empty());

        let related = crate::db::related::Related::new(&store);
        related.bump_pair("a", "b", NOW, 10).unwrap();
        related.bump_pair("a", "b", NOW, 10).unwrap();
        assert_eq!(association_weight(&store, "a", "b").unwrap(), Some(2));
        assert_eq!(association_weight(&store, "b", "a").unwrap(), None);
        assert_eq!(association_weight(&store, "a", "z").unwrap(), None);

        assert_eq!(relabel_memory(&store, "a", "c").unwrap(), 1);
        assert_eq!(relabel_memory(&store, "gone", "d").unwrap(), 0);
        assert!(relabel_memory(&store, "c", "b").is_err());
        assert_eq!(memory_ids(&store).unwrap(), ["b", "c"]);
        assert_eq!(
            memory_text(&store, "c", "content").unwrap().as_deref(),
            Some("y")
        );
    }
}
