//! Postgres-backed hub storage — the drop-in for an existing deployment.
//!
//! This backend's contract is stronger than "works": it must read and write
//! **the reference hub's own database**, including one restored from a dump of
//! the legacy schema. So the DDL, the column types, the `COLLATE "C"`, the
//! `memories_hub_seq` sequence and the migration are all deliberately the
//! reference's rather than a tidier equivalent. Where this file looks
//! over-specific, that is why.
//!
//! # Timeouts are not optional here
//!
//! Every request opens a fresh connection. A Postgres host that *hangs* — OOM,
//! full disk, frozen host — rather than cleanly refusing would otherwise block
//! each connect for the OS TCP timeout, in minutes. A handful of polled
//! `/health` checks then exhausts the thread pool and takes down every route,
//! including the one route documented to survive a database outage. Bounding
//! both connect time and statement time keeps the worst case in seconds.

use super::{
    stable_group_order, Counts, GraphPullQuery, HubStore, MemoryCounts, PullCursor, PullQuery,
    Stats, StoreError, StoreResult, NO_CATEGORY, UNATTRIBUTED,
};
use crate::canon::now_canonical;
use crate::record::Record;
use postgres::types::ToSql;
use postgres::{Client, NoTls};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 15_000;

/// The reference's schema, verbatim in shape.
///
/// `COLLATE "C"` on every key and timestamp column is the load-bearing part:
/// it makes byte comparison the ordering the keyset cursors assume. A database
/// created without it produces subtly wrong pull pages under any non-C locale.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id                TEXT COLLATE "C" PRIMARY KEY,
    content           TEXT NOT NULL,
    category          TEXT NOT NULL DEFAULT 'general',
    tags              JSONB NOT NULL DEFAULT '[]',
    source            TEXT NOT NULL DEFAULT 'manual',
    metadata          JSONB NOT NULL DEFAULT '{}',
    created_at        TEXT COLLATE "C" NOT NULL,
    updated_at        TEXT COLLATE "C" NOT NULL,
    capture_id        TEXT,
    node_id           TEXT,
    client            TEXT NOT NULL DEFAULT 'unknown',
    accessed_at       TEXT COLLATE "C",
    access_count      INTEGER NOT NULL DEFAULT 0,
    decay_rate        DOUBLE PRECISION NOT NULL DEFAULT 0.1,
    vitality          DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    base_weight       DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    status            TEXT NOT NULL DEFAULT 'active',
    memory_type       TEXT NOT NULL DEFAULT 'unclassified',
    source_capture_id TEXT,
    subject           TEXT,
    predicate         TEXT,
    "object"          TEXT,
    superseded_by     TEXT,
    deleted_at        TEXT COLLATE "C",
    origin_node       TEXT,
    hub_seq           BIGINT,
    sensitive         BOOLEAN NOT NULL DEFAULT false,
    remind_at         TEXT COLLATE "C"
);

CREATE SEQUENCE IF NOT EXISTS memories_hub_seq;

CREATE TABLE IF NOT EXISTS entities (
    id          TEXT COLLATE "C" PRIMARY KEY,
    name        TEXT NOT NULL,
    kind        TEXT,
    aliases     JSONB NOT NULL DEFAULT '[]',
    created_at  TEXT COLLATE "C" NOT NULL,
    updated_at  TEXT COLLATE "C" NOT NULL,
    node_id     TEXT,
    origin_node TEXT
);

CREATE TABLE IF NOT EXISTS memory_entities (
    memory_id  TEXT COLLATE "C" NOT NULL,
    entity_id  TEXT COLLATE "C" NOT NULL,
    created_at TEXT COLLATE "C" NOT NULL,
    PRIMARY KEY (memory_id, entity_id)
);

CREATE TABLE IF NOT EXISTS entity_relations (
    id                TEXT COLLATE "C" PRIMARY KEY,
    subject_entity_id TEXT COLLATE "C" NOT NULL,
    relation          TEXT NOT NULL,
    object_entity_id  TEXT COLLATE "C" NOT NULL,
    created_at        TEXT COLLATE "C" NOT NULL,
    updated_at        TEXT COLLATE "C" NOT NULL,
    node_id           TEXT,
    origin_node       TEXT
);

CREATE INDEX IF NOT EXISTS idx_memories_updated_at_id ON memories (updated_at, id);
CREATE INDEX IF NOT EXISTS idx_memories_hub_seq ON memories (hub_seq);
CREATE INDEX IF NOT EXISTS idx_entities_updated_at_id ON entities (updated_at, id);
CREATE INDEX IF NOT EXISTS idx_links_created_at ON memory_entities (created_at);
CREATE INDEX IF NOT EXISTS idx_entity_relations_created_at_id
    ON entity_relations (created_at, id);
"#;

/// Convert a legacy TIMESTAMPTZ column to the canonical TEXT form.
///
/// The output must match Python's `datetime.isoformat()` byte for byte: no
/// fractional part when microseconds are zero, and *exactly six digits* when
/// they are not.
///
/// # A deliberate divergence: the reference's regex is wrong here
///
/// The reference strips trailing zeros with `'\.?0+$'`, and its own comment
/// says the goal is to "match Python's `datetime.isoformat()` exactly". It
/// does for a zero fraction (`.000000` vanishes) and for one with no trailing
/// zeros (`.123456` survives) — but `.500000` becomes `.5`, which
/// `isoformat()` would never produce.
///
/// That is not cosmetic. Under `COLLATE "C"` the migrated `...:00.5+00:00`
/// sorts *before* the client's own `...:00.500000+00:00` (`+` is 0x2B, `0` is
/// 0x30), so a migrated row compares as older than the identical instant on
/// the node that wrote it — corrupting both the pull cursor's ordering and the
/// LWW comparison the hub resolves conflicts with.
///
/// So this strips only a wholly-zero fraction, which is what the reference
/// meant. Recorded in `docs/adr/0015`; caught by
/// `a_legacy_timestamptz_database_is_migrated_in_place`, which is the reason
/// that test runs against a real Postgres rather than being assumed.
const TS_CONVERT: &str = "regexp_replace(to_char({col} AT TIME ZONE 'UTC', \
     'YYYY-MM-DD\"T\"HH24:MI:SS.US'), '\\.000000$', '') || '+00:00'";

/// Columns added since the legacy hub schema, with client-matching defaults.
const NEW_MEMORY_COLUMNS: [(&str, &str); 17] = [
    ("accessed_at", "TEXT COLLATE \"C\""),
    ("access_count", "INTEGER NOT NULL DEFAULT 0"),
    ("decay_rate", "DOUBLE PRECISION NOT NULL DEFAULT 0.1"),
    ("vitality", "DOUBLE PRECISION NOT NULL DEFAULT 1.0"),
    ("base_weight", "DOUBLE PRECISION NOT NULL DEFAULT 1.0"),
    ("status", "TEXT NOT NULL DEFAULT 'active'"),
    ("memory_type", "TEXT NOT NULL DEFAULT 'unclassified'"),
    ("source_capture_id", "TEXT"),
    ("subject", "TEXT"),
    ("predicate", "TEXT"),
    ("\"object\"", "TEXT"),
    ("superseded_by", "TEXT"),
    ("deleted_at", "TEXT COLLATE \"C\""),
    ("origin_node", "TEXT"),
    ("hub_seq", "BIGINT"),
    ("sensitive", "BOOLEAN NOT NULL DEFAULT false"),
    ("remind_at", "TEXT COLLATE \"C\""),
];

const MEMORY_WIRE_COLUMNS: &str = r#"id, content, category, tags, source, metadata,
    created_at, updated_at, capture_id, node_id, client, accessed_at,
    access_count, decay_rate, vitality, base_weight, status, memory_type,
    source_capture_id, subject, predicate, "object", superseded_by, deleted_at,
    hub_seq, sensitive, remind_at"#;

pub struct PostgresStore {
    url: String,
    statement_timeout_ms: u64,
}

impl PostgresStore {
    /// Build a store against `url`. No connection is made until first use —
    /// every operation opens its own, matching the reference.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            statement_timeout_ms: std::env::var("REMIND_ME_HUB_STATEMENT_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(DEFAULT_STATEMENT_TIMEOUT_MS),
        }
    }

    fn connect(&self) -> StoreResult<Client> {
        let mut config: postgres::Config = self
            .url
            .parse()
            .map_err(|e| StoreError(format!("invalid DATABASE_URL: {e}")))?;
        config.connect_timeout(CONNECT_TIMEOUT);
        config.options(&format!(
            "-c statement_timeout={}",
            self.statement_timeout_ms
        ));
        config.connect(NoTls).map_err(err)
    }
}

fn err(e: postgres::Error) -> StoreError {
    StoreError(e.to_string())
}

fn opt_string(row: &postgres::Row, idx: usize) -> Option<String> {
    row.get::<_, Option<String>>(idx)
}

fn memory_row_to_json(row: &postgres::Row) -> Value {
    json!({
        "id": row.get::<_, String>(0),
        "content": row.get::<_, String>(1),
        "category": row.get::<_, String>(2),
        "tags": row.get::<_, Value>(3),
        "source": row.get::<_, String>(4),
        "metadata": row.get::<_, Value>(5),
        "created_at": row.get::<_, String>(6),
        "updated_at": row.get::<_, String>(7),
        "capture_id": opt_string(row, 8),
        "node_id": opt_string(row, 9),
        "client": row.get::<_, String>(10),
        "accessed_at": opt_string(row, 11),
        "access_count": row.get::<_, i32>(12),
        "decay_rate": row.get::<_, f64>(13),
        "vitality": row.get::<_, f64>(14),
        "base_weight": row.get::<_, f64>(15),
        "status": row.get::<_, String>(16),
        "memory_type": row.get::<_, String>(17),
        "source_capture_id": opt_string(row, 18),
        "subject": opt_string(row, 19),
        "predicate": opt_string(row, 20),
        "object": opt_string(row, 21),
        "superseded_by": opt_string(row, 22),
        "deleted_at": opt_string(row, 23),
        "hub_seq": row.get::<_, Option<i64>>(24),
        "sensitive": row.get::<_, bool>(25),
        "remind_at": opt_string(row, 26),
    })
}

impl HubStore for PostgresStore {
    fn migrate(&self) -> StoreResult<()> {
        let mut client = self.connect()?;

        // Detect a legacy database by column type before touching anything.
        let existing = client
            .query(
                "SELECT column_name, data_type, collation_name \
                 FROM information_schema.columns WHERE table_name = 'memories'",
                &[],
            )
            .map_err(err)?;
        let mut columns: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
        for row in &existing {
            columns.insert(
                row.get::<_, String>(0),
                (row.get::<_, String>(1), opt_string(row, 2)),
            );
        }

        for col in ["created_at", "updated_at", "accessed_at", "id"] {
            let Some((data_type, collation)) = columns.get(col) else {
                continue;
            };
            if data_type.starts_with("timestamp") {
                let expr = TS_CONVERT.replace("{col}", col);
                client
                    .batch_execute(&format!(
                        "ALTER TABLE memories ALTER COLUMN {col} TYPE TEXT COLLATE \"C\" \
                         USING {expr}"
                    ))
                    .map_err(err)?;
                eprintln!("hub: migrated memories.{col} from {data_type} to TEXT");
            } else if data_type == "text" && collation.as_deref() != Some("C") {
                client
                    .batch_execute(&format!(
                        "ALTER TABLE memories ALTER COLUMN {col} TYPE TEXT COLLATE \"C\""
                    ))
                    .map_err(err)?;
            }
        }

        if !columns.is_empty() {
            for (name, decl) in NEW_MEMORY_COLUMNS {
                client
                    .batch_execute(&format!(
                        "ALTER TABLE memories ADD COLUMN IF NOT EXISTS {name} {decl}"
                    ))
                    .map_err(err)?;
            }
            client
                .batch_execute(
                    "UPDATE memories SET accessed_at = created_at WHERE accessed_at IS NULL",
                )
                .map_err(err)?;
        }

        client.batch_execute(SCHEMA).map_err(err)?;
        client
            .batch_execute("ALTER TABLE entities ADD COLUMN IF NOT EXISTS origin_node TEXT")
            .map_err(err)?;

        // Backfill hub_seq in (updated_at, id) order, so a first migration
        // does not itself reorder existing history. Every write from here on
        // gets a fresh nextval() regardless of updated_at, which is what
        // decouples the pull cursor from client-authored timestamps.
        client
            .batch_execute(
                "UPDATE memories m SET hub_seq = sub.seq \
                   FROM (SELECT id, nextval('memories_hub_seq') AS seq FROM memories \
                          WHERE hub_seq IS NULL ORDER BY updated_at, id) sub \
                  WHERE m.id = sub.id",
            )
            .map_err(err)?;
        Ok(())
    }

    fn ping(&self) -> StoreResult<()> {
        let mut client = self.connect()?;
        client.query_one("SELECT 1", &[]).map_err(err)?;
        Ok(())
    }

    fn apply_record(&self, record: &Record, origin: Option<&str>) -> StoreResult<bool> {
        let mut client = self.connect()?;
        let mut tx = client.transaction().map_err(err)?;
        let applied = match record {
            Record::Memory(m) => {
                // Serialise memory writes until this transaction ends, so
                // `hub_seq` (handed out by `nextval()` at statement time)
                // becomes visible in the order it was assigned. Without it,
                // concurrent pushes commit out of sequence order and a puller
                // on the `Seq` cursor advances past a value that commits a
                // moment later, skipping that row for good.
                tx.execute("SELECT pg_advisory_xact_lock($1)", &[&HUB_SEQ_LOCK_KEY])
                    .map_err(err)?;
                let access_count = i32::try_from(m.access_count).unwrap_or(i32::MAX);
                let changed = tx
                    .execute(
                        MEMORY_UPSERT,
                        &[
                            &m.id,
                            &m.content,
                            &m.category,
                            &m.tags,
                            &m.source,
                            &m.metadata,
                            &m.created_at,
                            &m.updated_at,
                            &m.capture_id,
                            &m.node_id,
                            &m.client,
                            &m.accessed_at,
                            &access_count,
                            &m.decay_rate,
                            &m.vitality,
                            &m.base_weight,
                            &m.status,
                            &m.memory_type,
                            &m.source_capture_id,
                            &m.subject,
                            &m.predicate,
                            &m.object,
                            &m.superseded_by,
                            &m.deleted_at,
                            &origin,
                            &m.sensitive,
                            &m.remind_at,
                        ],
                    )
                    .map_err(err)?;
                changed > 0
            }
            Record::Entity(e) => apply_entity(&mut tx, e, origin)?,
            Record::Link(l) => {
                let changed = tx
                    .execute(
                        "INSERT INTO memory_entities (memory_id, entity_id, created_at) \
                         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                        &[&l.memory_id, &l.entity_id, &l.created_at],
                    )
                    .map_err(err)?;
                changed > 0
            }
            Record::EntityRelation(r) => {
                let changed = tx
                    .execute(
                        "INSERT INTO entity_relations (id, subject_entity_id, relation, \
                         object_entity_id, created_at, updated_at, node_id, origin_node) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
                        &[
                            &r.id,
                            &r.subject_entity_id,
                            &r.relation,
                            &r.object_entity_id,
                            &r.created_at,
                            &r.updated_at,
                            &r.node_id,
                            &origin,
                        ],
                    )
                    .map_err(err)?;
                changed > 0
            }
        };
        tx.commit().map_err(err)?;
        Ok(applied)
    }

    fn stats(&self) -> StoreResult<Stats> {
        let mut client = self.connect()?;
        let totals = client
            .query_one(
                "SELECT COUNT(*) AS total, \
                        COUNT(*) FILTER (WHERE deleted_at IS NOT NULL) AS tombstones, \
                        MIN(updated_at) AS oldest, MAX(updated_at) AS newest FROM memories",
                &[],
            )
            .map_err(err)?;

        Ok(Stats {
            total: totals.get::<_, i64>(0),
            tombstones: totals.get::<_, i64>(1),
            oldest_updated_at: opt_string(&totals, 2),
            newest_updated_at: opt_string(&totals, 3),
            by_origin_node: group_counts(
                &mut client,
                "SELECT COALESCE(NULLIF(origin_node, ''), $1), COUNT(*) FROM memories GROUP BY 1",
                &[&UNATTRIBUTED],
            )?,
            by_category: group_counts(
                &mut client,
                "SELECT COALESCE(NULLIF(category, ''), $1), COUNT(*) FROM memories GROUP BY 1",
                &[&NO_CATEGORY],
            )?,
            entities: scalar(&mut client, "SELECT COUNT(*) FROM entities")?,
            memory_entities: scalar(&mut client, "SELECT COUNT(*) FROM memory_entities")?,
            entity_relations: scalar(&mut client, "SELECT COUNT(*) FROM entity_relations")?,
        })
    }

    fn count_tables(&self, wanted: &[&str]) -> StoreResult<Counts> {
        let mut client = self.connect()?;
        let mut counts = Counts::default();
        for name in wanted {
            match *name {
                "memories" => {
                    let row = client
                        .query_one(
                            "SELECT COUNT(*), \
                             COUNT(*) FILTER (WHERE deleted_at IS NOT NULL) FROM memories",
                            &[],
                        )
                        .map_err(err)?;
                    let total: i64 = row.get(0);
                    let tombstones: i64 = row.get(1);
                    counts.memories = Some(MemoryCounts {
                        total,
                        live: Some(total - tombstones),
                        tombstones: Some(tombstones),
                    });
                }
                other => {
                    let n = scalar(&mut client, &format!("SELECT COUNT(*) FROM {other}"))?;
                    assign_scalar(&mut counts, other, n);
                }
            }
        }
        Ok(counts)
    }

    /// Planner estimates from `pg_class.reltuples` — O(1), no scan.
    ///
    /// Postgres cannot answer an unqualified `COUNT(*)` without scanning
    /// (MVCC visibility is per-transaction), and `/count` exists to be polled.
    /// `memories` reports `total` only: the live/tombstone split needs a
    /// filtered scan, which is the very thing being avoided, and estimating it
    /// would mean inventing a number.
    fn approx_count_tables(&self, wanted: &[&str]) -> StoreResult<Option<Counts>> {
        let mut client = self.connect()?;
        let names: Vec<String> = wanted.iter().map(|s| s.to_string()).collect();
        let rows = client
            .query(
                "SELECT relname, GREATEST(reltuples, 0)::bigint FROM pg_class \
                 WHERE relname = ANY($1) AND relkind = 'r'",
                &[&names],
            )
            .map_err(err)?;
        let mut estimates: BTreeMap<String, i64> = BTreeMap::new();
        for row in &rows {
            estimates.insert(row.get::<_, String>(0), row.get::<_, i64>(1));
        }

        let mut counts = Counts::default();
        for name in wanted {
            // A never-analysed table reports -1 in the catalog, already
            // normalised to 0 by GREATEST above; a table absent from the
            // catalog result is likewise 0 rather than missing.
            let estimate = estimates.get(*name).copied().unwrap_or(0);
            if *name == "memories" {
                counts.memories = Some(MemoryCounts {
                    total: estimate,
                    live: None,
                    tombstones: None,
                });
            } else {
                assign_scalar(&mut counts, name, estimate);
            }
        }
        Ok(Some(counts))
    }

    fn count_tables_since(&self, wanted: &[&str], since: &str) -> StoreResult<Counts> {
        let mut client = self.connect()?;
        let mut counts = Counts::default();
        for name in wanted {
            let column = match *name {
                "memories" | "entities" => "updated_at",
                _ => "created_at",
            };
            let row = client
                .query_one(
                    &format!("SELECT COUNT(*) FROM {name} WHERE {column} > $1"),
                    &[&since],
                )
                .map_err(err)?;
            let n: i64 = row.get(0);
            if *name == "memories" {
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
    }

    fn count_by_origin_node(&self, since: Option<&str>) -> StoreResult<Vec<(String, i64)>> {
        let mut client = self.connect()?;
        match since {
            Some(since) => group_counts(
                &mut client,
                "SELECT COALESCE(NULLIF(origin_node, ''), $1), COUNT(*) FROM memories \
                 WHERE updated_at > $2 GROUP BY 1",
                &[&UNATTRIBUTED, &since],
            ),
            None => group_counts(
                &mut client,
                "SELECT COALESCE(NULLIF(origin_node, ''), $1), COUNT(*) FROM memories GROUP BY 1",
                &[&UNATTRIBUTED],
            ),
        }
    }

    fn count_by_category(&self, since: Option<&str>) -> StoreResult<Vec<(String, i64)>> {
        let mut client = self.connect()?;
        match since {
            Some(since) => group_counts(
                &mut client,
                "SELECT COALESCE(NULLIF(category, ''), $1), COUNT(*) FROM memories \
                 WHERE updated_at > $2 GROUP BY 1",
                &[&NO_CATEGORY, &since],
            ),
            None => group_counts(
                &mut client,
                "SELECT COALESCE(NULLIF(category, ''), $1), COUNT(*) FROM memories GROUP BY 1",
                &[&NO_CATEGORY],
            ),
        }
    }

    fn compact_tombstones(&self, cutoff: &str) -> StoreResult<usize> {
        let mut client = self.connect()?;
        let mut tx = client.transaction().map_err(err)?;
        let rows = tx
            .query(
                "DELETE FROM memories WHERE deleted_at IS NOT NULL AND deleted_at < $1 \
                 RETURNING id",
                &[&cutoff],
            )
            .map_err(err)?;
        let ids: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
        if !ids.is_empty() {
            tx.execute(
                "DELETE FROM memory_entities WHERE memory_id = ANY($1)",
                &[&ids],
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)?;
        Ok(ids.len())
    }

    fn pull_memories(&self, query: &PullQuery) -> StoreResult<Vec<Value>> {
        let mut client = self.connect()?;
        let mut args: Vec<Box<dyn ToSql + Sync>> = Vec::new();
        let mut next = 1;
        let (mut where_clause, order_by) = match &query.cursor {
            PullCursor::Seq(seq) => {
                args.push(Box::new(*seq));
                let clause = format!("hub_seq > ${next}");
                next += 1;
                (clause, "hub_seq ASC")
            }
            PullCursor::Keyset { since, since_id } => {
                args.push(Box::new(since.clone()));
                args.push(Box::new(since.clone()));
                args.push(Box::new(since_id.clone()));
                let clause = format!(
                    "(updated_at > ${} OR (updated_at = ${} AND id > ${}))",
                    next,
                    next + 1,
                    next + 2
                );
                next += 3;
                (clause, "updated_at ASC, id ASC")
            }
            PullCursor::Since(since) => {
                args.push(Box::new(since.clone()));
                let clause = format!("updated_at > ${next}");
                next += 1;
                (clause, "updated_at ASC, id ASC")
            }
        };
        if let (Some(node), false) = (&query.exclude_node, query.full) {
            where_clause.push_str(&format!(
                " AND (origin_node IS NULL OR origin_node != ${next})"
            ));
            args.push(Box::new(node.clone()));
            next += 1;
        }
        args.push(Box::new(query.limit as i64));

        let sql = format!(
            "SELECT {MEMORY_WIRE_COLUMNS} FROM memories WHERE {where_clause} \
             ORDER BY {order_by} LIMIT ${next}"
        );
        let params: Vec<&(dyn ToSql + Sync)> = args.iter().map(|b| b.as_ref()).collect();
        let rows = client.query(&sql, &params).map_err(err)?;
        Ok(rows.iter().map(memory_row_to_json).collect())
    }

    fn pull_entities(&self, query: &PullQuery) -> StoreResult<Vec<Value>> {
        let mut client = self.connect()?;
        let (since, since_id) = match &query.cursor {
            PullCursor::Keyset { since, since_id } => (since.clone(), since_id.clone()),
            PullCursor::Since(since) => (since.clone(), String::new()),
            PullCursor::Seq(_) => (crate::EPOCH.to_string(), String::new()),
        };
        let mut args: Vec<Box<dyn ToSql + Sync>> =
            vec![Box::new(since.clone()), Box::new(since), Box::new(since_id)];
        let mut where_clause = "(updated_at > $1 OR (updated_at = $2 AND id > $3))".to_string();
        let mut next = 4;
        if let (Some(node), false) = (&query.exclude_node, query.full) {
            where_clause.push_str(&format!(
                " AND (origin_node IS NULL OR origin_node != ${next})"
            ));
            args.push(Box::new(node.clone()));
            next += 1;
        }
        args.push(Box::new(query.limit as i64));

        let sql = format!(
            "SELECT id, name, kind, aliases, created_at, updated_at, node_id FROM entities \
             WHERE {where_clause} ORDER BY updated_at ASC, id ASC LIMIT ${next}"
        );
        let params: Vec<&(dyn ToSql + Sync)> = args.iter().map(|b| b.as_ref()).collect();
        let rows = client.query(&sql, &params).map_err(err)?;
        Ok(rows
            .iter()
            .map(|row| {
                json!({
                    "record_type": "entity",
                    "id": row.get::<_, String>(0),
                    "name": row.get::<_, String>(1),
                    "kind": opt_string(row, 2),
                    "aliases": row.get::<_, Value>(3),
                    "created_at": row.get::<_, String>(4),
                    "updated_at": row.get::<_, String>(5),
                    "node_id": opt_string(row, 6),
                })
            })
            .collect())
    }

    fn pull_links(&self, query: &GraphPullQuery) -> StoreResult<Vec<Value>> {
        let mut client = self.connect()?;
        let rows = client
            .query(
                "SELECT memory_id, entity_id, created_at FROM memory_entities \
                 WHERE (created_at > $1 OR (created_at = $2 \
                        AND (memory_id || '|' || entity_id) > $3)) \
                 ORDER BY created_at ASC, (memory_id || '|' || entity_id) ASC LIMIT $4",
                &[
                    &query.since,
                    &query.since,
                    &query.since_id,
                    &(query.limit as i64),
                ],
            )
            .map_err(err)?;
        Ok(rows
            .iter()
            .map(|row| {
                let memory_id: String = row.get(0);
                let entity_id: String = row.get(1);
                json!({
                    "record_type": "memory_entity",
                    "id": format!("{memory_id}|{entity_id}"),
                    "memory_id": memory_id,
                    "entity_id": entity_id,
                    "created_at": row.get::<_, String>(2),
                })
            })
            .collect())
    }

    fn pull_entity_relations(&self, query: &GraphPullQuery) -> StoreResult<Vec<Value>> {
        let mut client = self.connect()?;
        let rows = client
            .query(
                "SELECT id, subject_entity_id, relation, object_entity_id, created_at, \
                 updated_at, node_id FROM entity_relations \
                 WHERE (created_at > $1 OR (created_at = $2 AND id > $3)) \
                 ORDER BY created_at ASC, id ASC LIMIT $4",
                &[
                    &query.since,
                    &query.since,
                    &query.since_id,
                    &(query.limit as i64),
                ],
            )
            .map_err(err)?;
        Ok(rows
            .iter()
            .map(|row| {
                json!({
                    "record_type": "entity_relation",
                    "id": row.get::<_, String>(0),
                    "subject_entity_id": row.get::<_, String>(1),
                    "relation": row.get::<_, String>(2),
                    "object_entity_id": row.get::<_, String>(3),
                    "created_at": row.get::<_, String>(4),
                    "updated_at": row.get::<_, String>(5),
                    "node_id": opt_string(row, 6),
                })
            })
            .collect())
    }
}

/// Key for the transaction-scoped advisory lock that orders memory writes by
/// `hub_seq` (see `apply_record`). Any fixed value works as long as nothing
/// else in the hub's database uses it; this one is ASCII "rrmhbseq".
const HUB_SEQ_LOCK_KEY: i64 = 0x7272_6d68_6273_6571;

/// Whole-row LWW on `updated_at`, with `hub_seq` bumped on every write.
///
/// The `WHERE EXCLUDED.updated_at > memories.updated_at` on the DO UPDATE is
/// what makes an older push a no-op rather than a clobber, and is also what
/// makes `execute` return 0 for an LWW loss — which the caller reports as
/// "not applied", distinct from "failed".
const MEMORY_UPSERT: &str = r#"
INSERT INTO memories
    (id, content, category, tags, source, metadata, created_at, updated_at,
     capture_id, node_id, client, accessed_at, access_count, decay_rate,
     vitality, base_weight, status, memory_type, source_capture_id,
     subject, predicate, "object", superseded_by, deleted_at, origin_node,
     hub_seq, sensitive, remind_at)
VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,
        $20,$21,$22,$23,$24,$25, nextval('memories_hub_seq'), $26, $27)
ON CONFLICT (id) DO UPDATE SET
    content           = EXCLUDED.content,
    category          = EXCLUDED.category,
    tags              = EXCLUDED.tags,
    source            = EXCLUDED.source,
    metadata          = EXCLUDED.metadata,
    updated_at        = EXCLUDED.updated_at,
    capture_id        = EXCLUDED.capture_id,
    node_id           = EXCLUDED.node_id,
    client            = EXCLUDED.client,
    accessed_at       = EXCLUDED.accessed_at,
    access_count      = EXCLUDED.access_count,
    decay_rate        = EXCLUDED.decay_rate,
    vitality          = EXCLUDED.vitality,
    base_weight       = EXCLUDED.base_weight,
    status            = EXCLUDED.status,
    memory_type       = EXCLUDED.memory_type,
    source_capture_id = EXCLUDED.source_capture_id,
    subject           = EXCLUDED.subject,
    predicate         = EXCLUDED.predicate,
    "object"          = EXCLUDED."object",
    superseded_by     = EXCLUDED.superseded_by,
    deleted_at        = EXCLUDED.deleted_at,
    origin_node       = EXCLUDED.origin_node,
    hub_seq           = nextval('memories_hub_seq'),
    sensitive         = EXCLUDED.sensitive,
    remind_at         = EXCLUDED.remind_at
WHERE EXCLUDED.updated_at > memories.updated_at
"#;

fn apply_entity(
    tx: &mut postgres::Transaction,
    e: &crate::record::EntityRecord,
    origin: Option<&str>,
) -> StoreResult<bool> {
    // FOR UPDATE: two nodes pushing the same entity concurrently must not both
    // read the same alias set and each write back their own union, losing one.
    let existing = tx
        .query_opt(
            "SELECT name, kind, aliases, updated_at FROM entities WHERE id = $1 FOR UPDATE",
            &[&e.id],
        )
        .map_err(err)?;

    let Some(row) = existing else {
        tx.execute(
            "INSERT INTO entities (id, name, kind, aliases, created_at, updated_at, \
             node_id, origin_node) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
            &[
                &e.id,
                &e.name,
                &e.kind,
                &json!(e.aliases),
                &e.created_at,
                &e.updated_at,
                &e.node_id,
                &origin,
            ],
        )
        .map_err(err)?;
        return Ok(true);
    };

    let local_kind: Option<String> = opt_string(&row, 1);
    let local_updated: String = row.get(3);
    let local_aliases: Vec<String> = match row.get::<_, Value>(2) {
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
            "UPDATE entities SET name = $1, kind = $2, aliases = $3, updated_at = $4, \
             node_id = $5, origin_node = $6 WHERE id = $7",
            &[
                &e.name,
                &e.kind.clone().or(local_kind),
                &json!(merged),
                &e.updated_at,
                &e.node_id,
                &origin,
                &e.id,
            ],
        )
        .map_err(err)?;
        return Ok(true);
    }

    let fill_kind = local_kind.clone().or_else(|| e.kind.clone());
    if merged != local_aliases || fill_kind != local_kind {
        tx.execute(
            "UPDATE entities SET aliases = $1, kind = $2, updated_at = $3, \
             origin_node = NULL WHERE id = $4",
            &[&json!(merged), &fill_kind, &now_canonical(), &e.id],
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

fn scalar(client: &mut Client, sql: &str) -> StoreResult<i64> {
    Ok(client.query_one(sql, &[]).map_err(err)?.get::<_, i64>(0))
}

fn group_counts(
    client: &mut Client,
    sql: &str,
    args: &[&(dyn ToSql + Sync)],
) -> StoreResult<Vec<(String, i64)>> {
    let rows = client.query(sql, args).map_err(err)?;
    let mut groups = BTreeMap::new();
    for row in &rows {
        groups.insert(row.get::<_, String>(0), row.get::<_, i64>(1));
    }
    Ok(stable_group_order(groups))
}
