//! The read-only reader for an old SQLite `memory.db` (ADR-0025).
//!
//! The node stores nothing in SQLite any more. This module is the one place
//! the crate still links `rusqlite`, and it only ever reads: it opens a file
//! read-only, hands the copy (`db::engine::copy`) each table's rows as JSON
//! maps, and lets the two foreign-file importers (`dbs_import`,
//! `mempalace_import`) query the other programs' SQLite databases they take
//! in (ADR-0023 §6). No other module names a SQLite type.
//!
//! The one write it has is [`fixture`], for tests that need a SQLite file to
//! read: it exists so no test module links `rusqlite` either.

use super::{Result, StoreError};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Map, Value};
use std::path::Path;

/// The schema version the last SQLite-storing build of this crate stamped
/// into `PRAGMA user_version`, and the one the engine's records mirror.
///
/// - 29: the Python reference's last schema.
/// - 30: vector chunks keyed by memory id, not `memories.rowid`.
/// - 31: no triggers; the repositories keep derived data in step.
/// - 32: context capture (`project`, `session_id`, …, `written_by`,
///   `capture_method` on `memories`; `memory_references` and `sessions` on
///   the engine only).
pub const SCHEMA_VERSION: i32 = 32;

/// The oldest schema the copy reads. v30 and v31 have the columns v32 has
/// minus the ones the engine defaults when they are missing; v29 keyed its
/// vectors on row numbers the copy has nowhere to put.
pub const OLDEST_COPIED_VERSION: i32 = 30;

/// One row, column name to value: text as a string, an integer or real as
/// a number, NULL as null, a blob as an array of bytes.
pub type Row = Map<String, Value>;

/// A SQLite file opened read-only.
pub struct LegacyDb {
    conn: Connection,
}

fn legacy_error(e: rusqlite::Error) -> StoreError {
    StoreError::Legacy(e.to_string())
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

/// A JSON value as the SQLite value to bind: a string as text, a number as
/// an integer when it is one and a real otherwise, a boolean as 0 or 1,
/// anything else as null.
fn binding(value: &Value) -> SqlValue {
    match value {
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or_default()),
        },
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        _ => SqlValue::Null,
    }
}

impl LegacyDb {
    /// Open the SQLite file at `path` read-only. The file is touched at
    /// once, so a path that is not a SQLite database fails here rather than
    /// at the first query.
    ///
    /// # Errors
    ///
    /// [`StoreError::Legacy`] if the file cannot be opened or read as a
    /// database.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(legacy_error)?;
        let db = Self { conn };
        // An empty database is a valid one; it just has no tables yet.
        db.conn
            .query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))
            .optional()
            .map_err(legacy_error)?;
        Ok(db)
    }

    /// What `PRAGMA user_version` reports.
    pub fn user_version(&self) -> Result<i32> {
        self.conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(legacy_error)
    }

    /// Whether the database has a table called `name`.
    pub fn has_table(&self, name: &str) -> Result<bool> {
        let found: Option<String> = self
            .conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
                [name],
                |r| r.get(0),
            )
            .optional()
            .map_err(legacy_error)?;
        Ok(found.is_some())
    }

    /// Every row of `table`, or none when there is no such table (the
    /// `promotions` table was created on first use).
    pub fn rows(&self, table: &str) -> Result<Vec<Row>> {
        self.rows_ordered(table, "")
    }

    /// [`LegacyDb::rows`], in the order `order_by` gives (an SQL `ORDER BY`
    /// clause, or empty).
    pub fn rows_ordered(&self, table: &str, order_by: &str) -> Result<Vec<Row>> {
        if !self.has_table(table)? {
            return Ok(Vec::new());
        }
        self.query(&format!("SELECT * FROM {table} {order_by}"), &[])
    }

    /// The rows `sql` returns with `bindings` bound in order, each as a
    /// map of column name to value.
    ///
    /// # Errors
    ///
    /// [`StoreError::Legacy`] if the statement cannot be prepared or run:
    /// a table it names is missing, or the file is not what it claims.
    pub fn query(&self, sql: &str, bindings: &[Value]) -> Result<Vec<Row>> {
        let mut stmt = self.conn.prepare(sql).map_err(legacy_error)?;
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let bound: Vec<SqlValue> = bindings.iter().map(binding).collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bound), |r| {
                let mut row = Row::new();
                for (i, name) in names.iter().enumerate() {
                    row.insert(name.clone(), json(r.get::<_, SqlValue>(i)?));
                }
                Ok(row)
            })
            .map_err(legacy_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(legacy_error)?;
        Ok(rows)
    }

    /// The first row `sql` returns, if any.
    pub fn query_one(&self, sql: &str, bindings: &[Value]) -> Result<Option<Row>> {
        Ok(self.query(sql, bindings)?.into_iter().next())
    }
}

/// Refuse a source the copy cannot read: older than
/// [`OLDEST_COPIED_VERSION`], which a build that still stored in SQLite has
/// to bring forward first, or newer than [`SCHEMA_VERSION`].
pub fn check_version(version: i32) -> Result<()> {
    if version > SCHEMA_VERSION {
        return Err(StoreError::Invalid(format!(
            "the SQLite store is at schema version {version}, newer than this build's \
             {SCHEMA_VERSION}: upgrade rusty-remind-me to copy it"
        )));
    }
    if version < OLDEST_COPIED_VERSION {
        return Err(StoreError::Invalid(format!(
            "the SQLite store is at schema version {version}; this build copies version \
             {OLDEST_COPIED_VERSION} and later. Open it once with rusty-remind-me 0.2.x, \
             which brings it to {SCHEMA_VERSION}, then start this build"
        )));
    }
    Ok(())
}

/// Test support: run `sql` against the SQLite file at `path`, creating the
/// file if it does not exist. The only write this crate makes to SQLite,
/// and only so a test can build the file another test reads.
///
/// # Errors
///
/// [`StoreError::Legacy`] if the file cannot be opened or `sql` fails.
#[doc(hidden)]
pub fn fixture(path: &Path, sql: &str) -> Result<()> {
    let conn = Connection::open(path).map_err(legacy_error)?;
    conn.execute_batch(sql).map_err(legacy_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_legacy_sqlite_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("legacy.db")
    }

    #[test]
    fn rows_read_every_column_as_json_and_a_missing_table_as_empty() {
        let path = scratch("rows");
        fixture(
            &path,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL, flag INTEGER, gone TEXT);
             INSERT INTO t (name, score, flag, gone) VALUES ('a', 1.5, 1, NULL);
             INSERT INTO t (name, score, flag, gone) VALUES ('b', 2.0, 0, 'x');
             PRAGMA user_version = 31;",
        )
        .unwrap();
        let db = LegacyDb::open(&path).unwrap();
        assert_eq!(db.user_version().unwrap(), 31);
        assert!(db.has_table("t").unwrap());
        assert!(!db.has_table("nope").unwrap());
        assert!(db.rows("nope").unwrap().is_empty());

        let rows = db.rows_ordered("t", "ORDER BY id DESC").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], Value::from("b"));
        assert_eq!(rows[0]["gone"], Value::from("x"));
        assert_eq!(rows[1]["score"], Value::from(1.5));
        assert_eq!(rows[1]["flag"], Value::from(1));
        assert_eq!(rows[1]["gone"], Value::Null);

        let one = db
            .query_one("SELECT name FROM t WHERE flag = ? AND score > ?", &[Value::from(0), Value::from(1.0)])
            .unwrap()
            .unwrap();
        assert_eq!(one["name"], Value::from("b"));
        assert!(db
            .query_one("SELECT name FROM t WHERE name = ?", &[Value::from("zz")])
            .unwrap()
            .is_none());
        assert!(matches!(
            db.query("SELECT * FROM nope", &[]),
            Err(StoreError::Legacy(_))
        ));
    }

    #[test]
    fn a_file_that_is_not_a_database_is_refused() {
        let path = scratch("junk");
        std::fs::write(&path, b"not a database at all").unwrap();
        assert!(matches!(LegacyDb::open(&path), Err(StoreError::Legacy(_))));
    }

    #[test]
    fn the_copy_takes_v30_to_v32_and_nothing_else() {
        for ok in [30, 31, 32] {
            assert!(check_version(ok).is_ok(), "{ok}");
        }
        for (bad, hint) in [(29, "0.2.x"), (3, "0.2.x"), (33, "upgrade")] {
            let refused = check_version(bad);
            assert!(
                matches!(&refused, Err(StoreError::Invalid(why)) if why.contains(hint)),
                "{refused:?}"
            );
        }
    }
}
