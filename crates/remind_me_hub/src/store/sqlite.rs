//! SQLite-backed hub storage.
//!
//! Wire-identical to the Postgres backend and schema-shaped the same way, but
//! it is *not* the same database: this is for a self-hosted hub that wants one
//! file and no server, not a drop-in for an existing Postgres deployment.
//!
//! # Where SQLite forces a real difference
//!
//! - **`hub_seq`.** Postgres has a sequence; SQLite does not. Issuing
//!   `MAX(hub_seq, high_water)+1` under the write lock gives the same monotonic
//!   property, because SQLite serialises writers anyway — the very thing that
//!   makes it the weaker choice for a busy hub is what makes this safe. The
//!   `hub_meta.high_water` half is what stops compaction, which deletes rows,
//!   from letting the number go backwards.
//! - **JSON columns.** Stored as TEXT holding JSON. The wire format is
//!   unchanged: [`crate::record`] has already parsed them into `Value`, and
//!   pulls parse them back out, so a client cannot tell.
//! - **No planner estimate.** `pg_class.reltuples` has no counterpart, so
//!   [`HubStore::approx_count_tables`] returns `None` and the route reports
//!   exact counts rather than labelling a scan "approximate".
//!
//! # Concurrency
//!
//! WAL, `busy_timeout`, and a mutex around the single connection. A hub is a
//! write-heavy central point, which is exactly SQLite's weak spot; the mutex
//! makes contention explicit and bounded rather than surfacing as
//! `SQLITE_BUSY` under load.

use super::{
    stable_group_order, Counts, GraphPullQuery, HubStore, MemoryCounts, PullCursor, PullQuery,
    Stats, StoreError, StoreResult, NO_CATEGORY, UNATTRIBUTED,
};
use crate::canon::now_canonical;
use crate::record::Record;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id                TEXT PRIMARY KEY,
    content           TEXT NOT NULL,
    category          TEXT NOT NULL DEFAULT 'general',
    tags              TEXT NOT NULL DEFAULT '[]',
    source            TEXT NOT NULL DEFAULT 'manual',
    metadata          TEXT NOT NULL DEFAULT '{}',
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    capture_id        TEXT,
    node_id           TEXT,
    client            TEXT NOT NULL DEFAULT 'unknown',
    accessed_at       TEXT,
    access_count      INTEGER NOT NULL DEFAULT 0,
    decay_rate        REAL NOT NULL DEFAULT 0.1,
    vitality          REAL NOT NULL DEFAULT 1.0,
    base_weight       REAL NOT NULL DEFAULT 1.0,
    status            TEXT NOT NULL DEFAULT 'active',
    memory_type       TEXT NOT NULL DEFAULT 'unclassified',
    source_capture_id TEXT,
    subject           TEXT,
    predicate         TEXT,
    "object"          TEXT,
    superseded_by     TEXT,
    deleted_at        TEXT,
    origin_node       TEXT,
    hub_seq           INTEGER,
    sensitive         INTEGER NOT NULL DEFAULT 0,
    remind_at         TEXT
);

CREATE TABLE IF NOT EXISTS entities (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    kind        TEXT,
    aliases     TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    node_id     TEXT,
    origin_node TEXT
);

CREATE TABLE IF NOT EXISTS memory_entities (
    memory_id  TEXT NOT NULL,
    entity_id  TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (memory_id, entity_id)
);

CREATE TABLE IF NOT EXISTS entity_relations (
    id                TEXT PRIMARY KEY,
    subject_entity_id TEXT NOT NULL,
    relation          TEXT NOT NULL,
    object_entity_id  TEXT NOT NULL,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    node_id           TEXT,
    origin_node       TEXT
);

-- The highest hub_seq ever issued, kept apart from `memories` because
-- compaction deletes rows and MAX(hub_seq) alone would then go backwards.
CREATE TABLE IF NOT EXISTS hub_meta (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    high_water INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_memories_updated_at_id ON memories (updated_at, id);
CREATE INDEX IF NOT EXISTS idx_memories_hub_seq ON memories (hub_seq);
CREATE INDEX IF NOT EXISTS idx_entities_updated_at_id ON entities (updated_at, id);
CREATE INDEX IF NOT EXISTS idx_links_created_at ON memory_entities (created_at);
CREATE INDEX IF NOT EXISTS idx_entity_relations_created_at_id
    ON entity_relations (created_at, id);
"#;

/// The wire columns for a memory. `origin_node` and `hub_seq` are hub
/// bookkeeping; `hub_seq` alone is on the wire, because a client needs it to
/// advance a `since_seq` cursor.
const MEMORY_WIRE_COLUMNS: &str = r#"id, content, category, tags, source, metadata,
    created_at, updated_at, capture_id, node_id, client, accessed_at,
    access_count, decay_rate, vitality, base_weight, status, memory_type,
    source_capture_id, subject, predicate, "object", superseded_by, deleted_at,
    hub_seq, sensitive, remind_at"#;

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (or create) a hub database at `path`.
    pub fn open(path: &str) -> StoreResult<Self> {
        let conn = Connection::open(path).map_err(err)?;
        // WAL so a reader (a pull) does not block a writer (a push). The busy
        // timeout covers the writer-vs-writer case the mutex below cannot,
        // namely a second *process* pointed at the same file.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(err)?;
        conn.pragma_update(None, "busy_timeout", 30000)
            .map_err(err)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(err)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// An in-memory hub, for tests.
    pub fn open_in_memory() -> StoreResult<Self> {
        let conn = Connection::open_in_memory().map_err(err)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> StoreResult<T>) -> StoreResult<T> {
        let mut guard = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }
}

fn err(e: rusqlite::Error) -> StoreError {
    StoreError(e.to_string())
}

/// The highest hub_seq ever issued: the larger of the live rows' maximum and
/// the recorded high-water mark.
///
/// Both halves are needed. `MAX(hub_seq)` alone goes backwards when
/// compaction deletes the newest row, and a node whose `since_seq` cursor
/// sits on the reissued number would then never pull the new row. The
/// high-water mark alone would miss rows written before it existed.
fn issued_seq(conn: &Connection) -> StoreResult<i64> {
    conn.query_row(
        "SELECT MAX(COALESCE((SELECT MAX(hub_seq) FROM memories), 0), \
                    COALESCE((SELECT high_water FROM hub_meta WHERE id = 1), 0))",
        [],
        |row| row.get(0),
    )
    .map_err(err)
}

/// Record `seq` as the high-water mark. Never lowers it.
fn raise_high_water(conn: &Connection, seq: i64) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO hub_meta (id, high_water) VALUES (1, ?1) \
         ON CONFLICT(id) DO UPDATE SET high_water = MAX(high_water, excluded.high_water)",
        params![seq],
    )
    .map_err(err)?;
    Ok(())
}

/// Next value for the hub sequence.
///
/// Safe under the write lock: SQLite serialises writers, so no two
/// transactions can read the same maximum and both commit. The caller raises
/// the high-water mark only once the write lands, in the same transaction, so
/// an LWW loser burns no number — the numbering stays gap-free, matching the
/// embedded-engine store (the differential suite compares seqs exactly).
fn next_seq(conn: &Connection) -> StoreResult<i64> {
    Ok(issued_seq(conn)? + 1)
}

fn json_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// Parse a TEXT column back into JSON, falling back to `default`.
///
/// A row written by something other than this hub could hold anything; a
/// malformed `tags` should degrade to `[]` on the wire rather than fail the
/// whole page.
fn json_column(raw: Option<String>, default: Value) -> Value {
    raw.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(default)
}

fn memory_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "content": row.get::<_, String>(1)?,
        "category": row.get::<_, String>(2)?,
        "tags": json_column(row.get::<_, Option<String>>(3)?, json!([])),
        "source": row.get::<_, String>(4)?,
        "metadata": json_column(row.get::<_, Option<String>>(5)?, json!({})),
        "created_at": row.get::<_, String>(6)?,
        "updated_at": row.get::<_, String>(7)?,
        "capture_id": row.get::<_, Option<String>>(8)?,
        "node_id": row.get::<_, Option<String>>(9)?,
        "client": row.get::<_, String>(10)?,
        "accessed_at": row.get::<_, Option<String>>(11)?,
        "access_count": row.get::<_, i64>(12)?,
        "decay_rate": row.get::<_, f64>(13)?,
        "vitality": row.get::<_, f64>(14)?,
        "base_weight": row.get::<_, f64>(15)?,
        "status": row.get::<_, String>(16)?,
        "memory_type": row.get::<_, String>(17)?,
        "source_capture_id": row.get::<_, Option<String>>(18)?,
        "subject": row.get::<_, Option<String>>(19)?,
        "predicate": row.get::<_, Option<String>>(20)?,
        "object": row.get::<_, Option<String>>(21)?,
        "superseded_by": row.get::<_, Option<String>>(22)?,
        "deleted_at": row.get::<_, Option<String>>(23)?,
        "hub_seq": row.get::<_, Option<i64>>(24)?,
        "sensitive": row.get::<_, bool>(25)?,
        "remind_at": row.get::<_, Option<String>>(26)?,
    }))
}

fn entity_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "record_type": "entity",
        "id": row.get::<_, String>(0)?,
        "name": row.get::<_, String>(1)?,
        "kind": row.get::<_, Option<String>>(2)?,
        "aliases": json_column(row.get::<_, Option<String>>(3)?, json!([])),
        "created_at": row.get::<_, String>(4)?,
        "updated_at": row.get::<_, String>(5)?,
        "node_id": row.get::<_, Option<String>>(6)?,
    }))
}

fn scalar_count(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> StoreResult<i64> {
    conn.query_row(sql, args, |row| row.get(0)).map_err(err)
}

impl HubStore for SqliteStore {
    fn migrate(&self) -> StoreResult<()> {
        self.with_conn(|conn| {
            conn.execute_batch(SCHEMA).map_err(err)?;
            // `CREATE TABLE IF NOT EXISTS` is a no-op on a database that
            // already has a `memories` table, so a column added to `SCHEMA`
            // after a hub has been deployed needs its own retrofit -- this is
            // that retrofit for `sensitive`/`remind_at` (#265), matching how
            // `origin_node` needed the same treatment historically.
            //
            // Unlike Postgres, SQLite's `ALTER TABLE ADD COLUMN` has no
            // `IF NOT EXISTS` clause -- confirmed the hard way, this used to
            // read `ADD COLUMN IF NOT EXISTS` and failed with a syntax error
            // on every existing database. `PRAGMA table_info` is the
            // idempotent check instead.
            let existing_columns: std::collections::HashSet<String> = conn
                .prepare("SELECT name FROM pragma_table_info('memories')")
                .map_err(err)?
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(err)?
                .collect::<rusqlite::Result<_>>()
                .map_err(err)?;
            for (name, decl) in [
                ("sensitive", "INTEGER NOT NULL DEFAULT 0"),
                ("remind_at", "TEXT"),
            ] {
                if !existing_columns.contains(name) {
                    conn.execute_batch(&format!("ALTER TABLE memories ADD COLUMN {name} {decl}"))
                        .map_err(err)?;
                }
            }
            // Backfill hub_seq for rows that predate it, in (updated_at, id)
            // order so a first migration does not itself reorder history.
            let missing: i64 = scalar_count(
                conn,
                "SELECT COUNT(*) FROM memories WHERE hub_seq IS NULL",
                &[],
            )?;
            if missing > 0 {
                let tx = conn.transaction().map_err(err)?;
                let ids: Vec<String> = {
                    let mut stmt = tx
                        .prepare(
                            "SELECT id FROM memories WHERE hub_seq IS NULL \
                             ORDER BY updated_at, id",
                        )
                        .map_err(err)?;
                    let rows = stmt
                        .query_map([], |row| row.get::<_, String>(0))
                        .map_err(err)?;
                    rows.collect::<rusqlite::Result<Vec<String>>>()
                        .map_err(err)?
                };
                let base = issued_seq(&tx)?;
                for (offset, id) in ids.iter().enumerate() {
                    tx.execute(
                        "UPDATE memories SET hub_seq = ?1 WHERE id = ?2",
                        params![base + offset as i64 + 1, id],
                    )
                    .map_err(err)?;
                }
                tx.commit().map_err(err)?;
            }
            // Seed (or catch up) the high-water mark from the rows present,
            // so a hub upgraded from before `hub_meta` existed is protected
            // from its very first compaction. Idempotent: it never lowers the
            // mark. A number already reissued before this upgrade cannot be
            // recovered here; only a `full` pull heals a node stuck on it.
            let issued = issued_seq(conn)?;
            raise_high_water(conn, issued)?;
            Ok(())
        })
    }

    fn ping(&self) -> StoreResult<()> {
        self.with_conn(|conn| {
            conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .map_err(err)?;
            Ok(())
        })
    }

    fn apply_record(&self, record: &Record, origin: Option<&str>) -> StoreResult<bool> {
        self.with_conn(|conn| {
            let tx = conn.transaction().map_err(err)?;
            let applied = match record {
                Record::Memory(m) => {
                    let seq = next_seq(&tx)?;
                    // LWW: the WHERE on the upsert is what makes an older
                    // record a no-op rather than a clobber.
                    let changed = tx
                        .execute(
                            r#"INSERT INTO memories
                                (id, content, category, tags, source, metadata,
                                 created_at, updated_at, capture_id, node_id, client,
                                 accessed_at, access_count, decay_rate, vitality,
                                 base_weight, status, memory_type, source_capture_id,
                                 subject, predicate, "object", superseded_by,
                                 deleted_at, origin_node, hub_seq, sensitive, remind_at)
                               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,
                                       ?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,
                                       ?27,?28)
                               ON CONFLICT(id) DO UPDATE SET
                                 content=excluded.content,
                                 category=excluded.category,
                                 tags=excluded.tags,
                                 source=excluded.source,
                                 metadata=excluded.metadata,
                                 updated_at=excluded.updated_at,
                                 capture_id=excluded.capture_id,
                                 node_id=excluded.node_id,
                                 client=excluded.client,
                                 accessed_at=excluded.accessed_at,
                                 access_count=excluded.access_count,
                                 decay_rate=excluded.decay_rate,
                                 vitality=excluded.vitality,
                                 base_weight=excluded.base_weight,
                                 status=excluded.status,
                                 memory_type=excluded.memory_type,
                                 source_capture_id=excluded.source_capture_id,
                                 subject=excluded.subject,
                                 predicate=excluded.predicate,
                                 "object"=excluded."object",
                                 superseded_by=excluded.superseded_by,
                                 deleted_at=excluded.deleted_at,
                                 origin_node=excluded.origin_node,
                                 hub_seq=excluded.hub_seq,
                                 sensitive=excluded.sensitive,
                                 remind_at=excluded.remind_at
                               WHERE excluded.updated_at > memories.updated_at"#,
                            params![
                                m.id,
                                m.content,
                                m.category,
                                json_text(&m.tags),
                                m.source,
                                json_text(&m.metadata),
                                m.created_at,
                                m.updated_at,
                                m.capture_id,
                                m.node_id,
                                m.client,
                                m.accessed_at,
                                m.access_count,
                                m.decay_rate,
                                m.vitality,
                                m.base_weight,
                                m.status,
                                m.memory_type,
                                m.source_capture_id,
                                m.subject,
                                m.predicate,
                                m.object,
                                m.superseded_by,
                                m.deleted_at,
                                origin,
                                seq,
                                m.sensitive,
                                m.remind_at,
                            ],
                        )
                        .map_err(err)?;
                    if changed > 0 {
                        raise_high_water(&tx, seq)?;
                    }
                    changed > 0
                }
                Record::Entity(e) => apply_entity(&tx, e, origin)?,
                Record::Link(l) => {
                    let changed = tx
                        .execute(
                            "INSERT INTO memory_entities (memory_id, entity_id, created_at) \
                             VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
                            params![l.memory_id, l.entity_id, l.created_at],
                        )
                        .map_err(err)?;
                    changed > 0
                }
                Record::EntityRelation(r) => {
                    let changed = tx
                        .execute(
                            "INSERT INTO entity_relations (id, subject_entity_id, relation, \
                             object_entity_id, created_at, updated_at, node_id, origin_node) \
                             VALUES (?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT DO NOTHING",
                            params![
                                r.id,
                                r.subject_entity_id,
                                r.relation,
                                r.object_entity_id,
                                r.created_at,
                                r.updated_at,
                                r.node_id,
                                origin,
                            ],
                        )
                        .map_err(err)?;
                    changed > 0
                }
            };
            tx.commit().map_err(err)?;
            Ok(applied)
        })
    }

    fn stats(&self) -> StoreResult<Stats> {
        self.with_conn(|conn| {
            let (total, tombstones, oldest, newest) = conn
                .query_row(
                    "SELECT COUNT(*), \
                            COALESCE(SUM(CASE WHEN deleted_at IS NOT NULL THEN 1 ELSE 0 END), 0), \
                            MIN(updated_at), MAX(updated_at) FROM memories",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
                .map_err(err)?;

            Ok(Stats {
                total,
                tombstones,
                oldest_updated_at: oldest,
                newest_updated_at: newest,
                by_origin_node: group_counts(
                    conn,
                    "SELECT COALESCE(NULLIF(origin_node, ''), ?1), COUNT(*) \
                     FROM memories GROUP BY 1",
                    UNATTRIBUTED,
                )?,
                by_category: group_counts(
                    conn,
                    "SELECT COALESCE(NULLIF(category, ''), ?1), COUNT(*) \
                     FROM memories GROUP BY 1",
                    NO_CATEGORY,
                )?,
                entities: scalar_count(conn, "SELECT COUNT(*) FROM entities", &[])?,
                memory_entities: scalar_count(conn, "SELECT COUNT(*) FROM memory_entities", &[])?,
                entity_relations: scalar_count(conn, "SELECT COUNT(*) FROM entity_relations", &[])?,
            })
        })
    }

    fn count_tables(&self, wanted: &[&str]) -> StoreResult<Counts> {
        self.with_conn(|conn| {
            let mut counts = Counts::default();
            for name in wanted {
                match *name {
                    "memories" => {
                        let (total, tombstones) = conn
                            .query_row(
                                "SELECT COUNT(*), COALESCE(SUM(CASE WHEN deleted_at IS NOT NULL \
                                 THEN 1 ELSE 0 END), 0) FROM memories",
                                [],
                                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                            )
                            .map_err(err)?;
                        counts.memories = Some(MemoryCounts {
                            total,
                            live: Some(total - tombstones),
                            tombstones: Some(tombstones),
                        });
                    }
                    other => {
                        // `other` is always from COUNTABLE -- the route
                        // rejects anything else before reaching storage.
                        let n = scalar_count(conn, &format!("SELECT COUNT(*) FROM {other}"), &[])?;
                        assign_scalar(&mut counts, other, n);
                    }
                }
            }
            Ok(counts)
        })
    }

    /// SQLite has no planner row estimate worth reporting.
    ///
    /// `sqlite_stat1` only exists after `ANALYZE` and stores a *string* whose
    /// leading field is a row estimate — stale by construction and absent on a
    /// database nobody has analysed. Reporting `None` makes the route fall
    /// back to exact counts, which is honest; the alternative is labelling a
    /// full scan "approximate", which is the one thing the flag must not mean.
    fn approx_count_tables(&self, _wanted: &[&str]) -> StoreResult<Option<Counts>> {
        Ok(None)
    }

    fn count_tables_since(&self, wanted: &[&str], since: &str) -> StoreResult<Counts> {
        self.with_conn(|conn| {
            let mut counts = Counts::default();
            for name in wanted {
                let column = match *name {
                    "memories" | "entities" => "updated_at",
                    _ => "created_at",
                };
                let n = scalar_count(
                    conn,
                    &format!("SELECT COUNT(*) FROM {name} WHERE {column} > ?1"),
                    &[&since],
                )?;
                if *name == "memories" {
                    // No live/tombstone split: a tombstone written in the
                    // window *is* a record that changed in the window, and
                    // splitting invites reading this as a live-record delta.
                    counts.memories = Some(MemoryCounts {
                        total: n,
                        live: None,
                        tombstones: None,
                    });
                } else {
                    assign_scalar(&mut counts, name, n);
                }
            }
            Ok(counts)
        })
    }

    fn count_by_origin_node(&self, since: Option<&str>) -> StoreResult<Vec<(String, i64)>> {
        self.with_conn(|conn| match since {
            Some(since) => group_counts_with(
                conn,
                "SELECT COALESCE(NULLIF(origin_node, ''), ?1), COUNT(*) FROM memories \
                 WHERE updated_at > ?2 GROUP BY 1",
                &[&UNATTRIBUTED, &since],
            ),
            None => group_counts(
                conn,
                "SELECT COALESCE(NULLIF(origin_node, ''), ?1), COUNT(*) FROM memories GROUP BY 1",
                UNATTRIBUTED,
            ),
        })
    }

    fn count_by_category(&self, since: Option<&str>) -> StoreResult<Vec<(String, i64)>> {
        self.with_conn(|conn| match since {
            Some(since) => group_counts_with(
                conn,
                "SELECT COALESCE(NULLIF(category, ''), ?1), COUNT(*) FROM memories \
                 WHERE updated_at > ?2 GROUP BY 1",
                &[&NO_CATEGORY, &since],
            ),
            None => group_counts(
                conn,
                "SELECT COALESCE(NULLIF(category, ''), ?1), COUNT(*) FROM memories GROUP BY 1",
                NO_CATEGORY,
            ),
        })
    }

    fn compact_tombstones(&self, cutoff: &str) -> StoreResult<usize> {
        self.with_conn(|conn| {
            let tx = conn.transaction().map_err(err)?;
            let ids: Vec<String> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT id FROM memories WHERE deleted_at IS NOT NULL AND deleted_at < ?1",
                    )
                    .map_err(err)?;
                let rows = stmt
                    .query_map(params![cutoff], |row| row.get::<_, String>(0))
                    .map_err(err)?;
                rows.collect::<rusqlite::Result<Vec<String>>>()
                    .map_err(err)?
            };
            if !ids.is_empty() {
                // Pin the high-water mark before the rows that may hold it go.
                // `next_seq` already keeps it current; this is the backstop for
                // a mark that somehow lags the rows.
                let issued = issued_seq(&tx)?;
                raise_high_water(&tx, issued)?;
                let placeholders = vec!["?"; ids.len()].join(",");
                tx.execute(
                    &format!("DELETE FROM memories WHERE id IN ({placeholders})"),
                    params_from_iter(ids.iter()),
                )
                .map_err(err)?;
                tx.execute(
                    &format!("DELETE FROM memory_entities WHERE memory_id IN ({placeholders})"),
                    params_from_iter(ids.iter()),
                )
                .map_err(err)?;
            }
            tx.commit().map_err(err)?;
            Ok(ids.len())
        })
    }

    fn pull_memories(&self, query: &PullQuery) -> StoreResult<Vec<Value>> {
        self.with_conn(|conn| {
            let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            let (where_clause, order_by) = match &query.cursor {
                PullCursor::Seq(seq) => {
                    args.push(Box::new(*seq));
                    ("hub_seq > ?".to_string(), "hub_seq ASC")
                }
                PullCursor::Keyset { since, since_id } => {
                    args.push(Box::new(since.clone()));
                    args.push(Box::new(since.clone()));
                    args.push(Box::new(since_id.clone()));
                    (
                        "(updated_at > ? OR (updated_at = ? AND id > ?))".to_string(),
                        "updated_at ASC, id ASC",
                    )
                }
                PullCursor::Since(since) => {
                    args.push(Box::new(since.clone()));
                    ("updated_at > ?".to_string(), "updated_at ASC, id ASC")
                }
            };
            let mut where_clause = where_clause;
            if let (Some(node), false) = (&query.exclude_node, query.full) {
                where_clause.push_str(" AND (origin_node IS NULL OR origin_node != ?)");
                args.push(Box::new(node.clone()));
            }
            args.push(Box::new(query.limit as i64));

            let sql = format!(
                "SELECT {MEMORY_WIRE_COLUMNS} FROM memories WHERE {where_clause} \
                 ORDER BY {order_by} LIMIT ?"
            );
            let mut stmt = conn.prepare(&sql).map_err(err)?;
            let rows = stmt
                .query_map(params_from_iter(args.iter().map(|a| a.as_ref())), |row| {
                    memory_row_to_json(row)
                })
                .map_err(err)?;
            rows.collect::<rusqlite::Result<Vec<Value>>>().map_err(err)
        })
    }

    fn pull_entities(&self, query: &PullQuery) -> StoreResult<Vec<Value>> {
        self.with_conn(|conn| {
            let (since, since_id) = match &query.cursor {
                PullCursor::Keyset { since, since_id } => (since.clone(), since_id.clone()),
                PullCursor::Since(since) => (since.clone(), String::new()),
                // Entities have no hub_seq; the reference's entity pull has no
                // since_seq mode at all, so a seq cursor degrades to the epoch
                // rather than silently returning nothing.
                PullCursor::Seq(_) => (crate::EPOCH.to_string(), String::new()),
            };
            let mut args: Vec<Box<dyn rusqlite::ToSql>> =
                vec![Box::new(since.clone()), Box::new(since), Box::new(since_id)];
            let mut where_clause = "(updated_at > ? OR (updated_at = ? AND id > ?))".to_string();
            if let (Some(node), false) = (&query.exclude_node, query.full) {
                where_clause.push_str(" AND (origin_node IS NULL OR origin_node != ?)");
                args.push(Box::new(node.clone()));
            }
            args.push(Box::new(query.limit as i64));

            let sql = format!(
                "SELECT id, name, kind, aliases, created_at, updated_at, node_id \
                 FROM entities WHERE {where_clause} ORDER BY updated_at ASC, id ASC LIMIT ?"
            );
            let mut stmt = conn.prepare(&sql).map_err(err)?;
            let rows = stmt
                .query_map(params_from_iter(args.iter().map(|a| a.as_ref())), |row| {
                    entity_row_to_json(row)
                })
                .map_err(err)?;
            rows.collect::<rusqlite::Result<Vec<Value>>>().map_err(err)
        })
    }

    fn pull_links(&self, query: &GraphPullQuery) -> StoreResult<Vec<Value>> {
        self.with_conn(|conn| {
            // The synthetic key appears in both the filter and the ORDER BY so
            // server ordering and the client's cursor comparison agree exactly.
            let mut stmt = conn
                .prepare(
                    "SELECT memory_id, entity_id, created_at FROM memory_entities \
                     WHERE (created_at > ?1 OR (created_at = ?2 \
                            AND (memory_id || '|' || entity_id) > ?3)) \
                     ORDER BY created_at ASC, (memory_id || '|' || entity_id) ASC LIMIT ?4",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map(
                    params![query.since, query.since, query.since_id, query.limit as i64],
                    |row| {
                        let memory_id: String = row.get(0)?;
                        let entity_id: String = row.get(1)?;
                        Ok(json!({
                            "record_type": "memory_entity",
                            "id": format!("{memory_id}|{entity_id}"),
                            "memory_id": memory_id,
                            "entity_id": entity_id,
                            "created_at": row.get::<_, String>(2)?,
                        }))
                    },
                )
                .map_err(err)?;
            rows.collect::<rusqlite::Result<Vec<Value>>>().map_err(err)
        })
    }

    fn pull_entity_relations(&self, query: &GraphPullQuery) -> StoreResult<Vec<Value>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, subject_entity_id, relation, object_entity_id, created_at, \
                     updated_at, node_id FROM entity_relations \
                     WHERE (created_at > ?1 OR (created_at = ?2 AND id > ?3)) \
                     ORDER BY created_at ASC, id ASC LIMIT ?4",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map(
                    params![query.since, query.since, query.since_id, query.limit as i64],
                    |row| {
                        Ok(json!({
                            "record_type": "entity_relation",
                            "id": row.get::<_, String>(0)?,
                            "subject_entity_id": row.get::<_, String>(1)?,
                            "relation": row.get::<_, String>(2)?,
                            "object_entity_id": row.get::<_, String>(3)?,
                            "created_at": row.get::<_, String>(4)?,
                            "updated_at": row.get::<_, String>(5)?,
                            "node_id": row.get::<_, Option<String>>(6)?,
                        }))
                    },
                )
                .map_err(err)?;
            rows.collect::<rusqlite::Result<Vec<Value>>>().map_err(err)
        })
    }
}

/// Entity upsert: LWW on `updated_at`, aliases always union-merged.
///
/// The union merge happens regardless of which side wins, because union is
/// commutative and idempotent, so every node converges on the same alias set
/// without needing to agree on an order.
fn apply_entity(
    tx: &rusqlite::Transaction,
    e: &crate::record::EntityRecord,
    origin: Option<&str>,
) -> StoreResult<bool> {
    let existing = tx
        .query_row(
            "SELECT name, kind, aliases, updated_at FROM entities WHERE id = ?1",
            params![e.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(err)?;

    let Some((_name, local_kind, local_aliases_raw, local_updated)) = existing else {
        tx.execute(
            "INSERT INTO entities (id, name, kind, aliases, created_at, updated_at, \
             node_id, origin_node) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                e.id,
                e.name,
                e.kind,
                json_text(&json!(e.aliases)),
                e.created_at,
                e.updated_at,
                e.node_id,
                origin,
            ],
        )
        .map_err(err)?;
        return Ok(true);
    };

    let local_aliases: Vec<String> = match json_column(local_aliases_raw, json!([])) {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) if !s.is_empty() => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut merged = local_aliases.clone();
    for alias in &e.aliases {
        if !merged.contains(alias) {
            merged.push(alias.clone());
        }
    }

    if e.updated_at > local_updated {
        tx.execute(
            "UPDATE entities SET name = ?1, kind = ?2, aliases = ?3, updated_at = ?4, \
             node_id = ?5, origin_node = ?6 WHERE id = ?7",
            params![
                e.name,
                e.kind.clone().or(local_kind),
                json_text(&json!(merged)),
                e.updated_at,
                e.node_id,
                origin,
                e.id,
            ],
        )
        .map_err(err)?;
        return Ok(true);
    }

    // LWW-losing enrichment. The peer protocol leaves `updated_at` alone, but
    // the hub is pull-only: without a bump, nodes whose cursor has already
    // passed this entity would never see the merged aliases. Bumping is safe
    // because union-merge is idempotent -- a re-pulled merge that changes
    // nothing does not bump again, so the cycle terminates.
    let fill_kind = local_kind.clone().or_else(|| e.kind.clone());
    if merged != local_aliases || fill_kind != local_kind {
        tx.execute(
            "UPDATE entities SET aliases = ?1, kind = ?2, updated_at = ?3, \
             origin_node = NULL WHERE id = ?4",
            params![json_text(&json!(merged)), fill_kind, now_canonical(), e.id],
        )
        .map_err(err)?;
        return Ok(true);
    }
    Ok(false)
}

fn assign_scalar(counts: &mut Counts, name: &str, n: i64) {
    match name {
        "entities" => counts.entities = Some(n),
        "memory_entities" => counts.memory_entities = Some(n),
        "entity_relations" => counts.entity_relations = Some(n),
        _ => {}
    }
}

fn group_counts(conn: &Connection, sql: &str, fallback: &str) -> StoreResult<Vec<(String, i64)>> {
    group_counts_with(conn, sql, &[&fallback])
}

fn group_counts_with(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> StoreResult<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(sql).map_err(err)?;
    let rows = stmt
        .query_map(args, |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(err)?;
    let mut groups = BTreeMap::new();
    for row in rows {
        let (key, count) = row.map_err(err)?;
        groups.insert(key, count);
    }
    Ok(stable_group_order(groups))
}

#[cfg(test)]
mod tests {
    //! What is only true of this backend. The protocol itself is the route
    //! suite's (`tests/hub_routes_test.rs`), which runs against this store too.

    use super::*;
    use crate::record;

    fn memory(id: &str, updated: &str) -> Record {
        record::parse(&json!({
            "id": id,
            "content": format!("content of {id}"),
            "created_at": "2026-08-01T00:00:00Z",
            "updated_at": updated,
        }))
        .expect("a valid memory")
    }

    fn seq_of(store: &SqliteStore, id: &str) -> i64 {
        store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT hub_seq FROM memories WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .map_err(err)
            })
            .expect("the memory exists")
    }

    #[test]
    fn a_hub_upgraded_from_before_hub_meta_does_not_reissue_after_compaction() {
        // A database written by a hub that predates `hub_meta`: two rows,
        // the newest an expired tombstone, and no high-water mark anywhere.
        let store = SqliteStore::open_in_memory().unwrap();
        store.migrate().unwrap();
        store
            .with_conn(|conn| {
                conn.execute_batch(
                    "DROP TABLE hub_meta;
                     INSERT INTO memories (id, content, created_at, updated_at, hub_seq)
                         VALUES ('m1', 'a', '2026-08-01T00:00:00Z', '2026-08-01T00:00:00Z', 1);
                     INSERT INTO memories
                         (id, content, created_at, updated_at, deleted_at, hub_seq)
                         VALUES ('m2', 'b', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z',
                                 '2026-01-01T00:00:00Z', 2);",
                )
                .map_err(err)
            })
            .unwrap();

        store.migrate().unwrap();
        assert_eq!(store.compact_tombstones("2026-06-01T00:00:00Z").unwrap(), 1);
        store
            .apply_record(&memory("m3", "2026-08-02T00:00:00Z"), None)
            .unwrap();

        assert_eq!(seq_of(&store, "m3"), 3, "seq 2 was issued once already");
    }

    #[test]
    fn migrate_twice_keeps_the_high_water_mark() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.migrate().unwrap();
        store
            .apply_record(&memory("m1", "2026-08-01T00:00:00Z"), None)
            .unwrap();
        store.migrate().unwrap();
        store.migrate().unwrap();
        let mark: i64 = store
            .with_conn(|conn| {
                conn.query_row("SELECT high_water FROM hub_meta WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .map_err(err)
            })
            .unwrap();
        assert_eq!(mark, 1);
    }
}
