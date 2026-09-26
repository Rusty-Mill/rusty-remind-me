//! Schema parity and reconciliation.
//!
//! The earlier version of these tests checked table *names* and `memories`
//! columns. That let four tables diverge in their columns and constraints
//! unnoticed — the check was shaped like the mistake. These compare the whole
//! schema: every table, index and trigger, by normalised DDL.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::backup::list_backups;
use remind_me_core::db::migrations::SCHEMA_VERSION;
use remind_me_core::db::queries;
use remind_me_core::embedder::{EMBEDDING_BACKEND_ENV, EMBEDDING_DIM_ENV, OLLAMA_MODEL_ENV};
use remind_me_core::sync::{HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// `writes_still_reach_the_sync_outbox` is the only test here that touches
/// the sync env vars (`#76`'s `sync_flags` gate means a write only reaches
/// the outbox when sync is actually configured) — held for consistency with
/// every other file that touches them, not because another test in this file
/// currently races on it.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The schema files, as shipped: what every open must converge on.
const SCHEMA_TABLES: &str = include_str!("../src/db/schema_tables.sql");
const SCHEMA_INDEXES: &str = include_str!("../src/db/schema_indexes.sql");
/// The triggers schema v30 and earlier carried, for building older databases.
const V30_TRIGGERS: &str = include_str!("fixtures/schema_v30_triggers.sql");

struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(
            format!(
                "rmm_schema_{}_{}_{:?}",
                tag,
                std::process::id(),
                std::thread::current().id()
            )
            .replace(['(', ')', ' '], ""),
        );
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.join("s.db"))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(p) = self.0.parent() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

/// Collapse whitespace, strip comments and `IF NOT EXISTS`, lowercase — so two
/// DDL strings compare equal when they describe the same object.
fn normalise(sql: &str) -> String {
    let no_comments: String = sql
        .lines()
        .map(|l| l.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    no_comments
        .replace("IF NOT EXISTS ", "")
        // ALTER TABLE ... RENAME stores the new name quoted, so a rebuilt table
        // reads back as CREATE TABLE "memories". Same object, different text.
        .replace('"', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Every object of `kind`, keyed by name, with normalised DDL. Excludes the
/// shadow tables FTS5 manages itself.
fn objects(conn: &Connection, kind: &str) -> BTreeMap<String, String> {
    let mut stmt = conn
        .prepare("SELECT name, sql FROM sqlite_master WHERE type = ? AND sql IS NOT NULL")
        .unwrap();
    stmt.query_map([kind], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .filter(|(name, _)| {
        !name.starts_with("sqlite_")
            && !["_config", "_data", "_docsize", "_idx", "_content"]
                .iter()
                .any(|s| name.ends_with(s))
    })
    .map(|(name, sql)| (name, normalise(&sql)))
    .collect()
}

/// Tables this crate's own code creates, deliberately, beyond the schema
/// files, so their presence is expected rather than a drift
/// `assert_matches_schema` should flag. They were kept out of the files while
/// those were generated from the Python `remind_me` (retired, ADR-0023).
///
/// `import_archives` / `import_archive_spans` / `idx_archive_spans_import`:
/// raw-transcript retention (#212). The obvious home for the archive path was
/// a column on `chat_imports`, which is exactly what this list existed to
/// prevent — `schema_tables.sql` was generated verbatim then, so the column
/// would have been reverted by the next regeneration. Target-only
/// tables created by `db::archives::ensure_tables` instead.
/// `promotions` / `idx_promotions_source`: the refinement ladder's provenance
/// (#208), linking a promoted artifact to the memories it was distilled from.
const OWN_ADDITIONS: &[&str] = &[
    "import_archives",
    "import_archive_spans",
    "idx_archive_spans_import",
    "promotions",
    "idx_promotions_source",
];

/// Compare live objects of `kind` against the shipped schema, reporting only
/// what differs. A whole-map `assert_eq!` dumps twenty tables of DDL and buries
/// the one that is wrong.
fn assert_matches_schema(live: &Connection, kind: &str) {
    let actual = objects(live, kind);
    let want = objects(&expected(), kind);

    let mut problems = Vec::new();
    for (name, want_sql) in &want {
        match actual.get(name) {
            None => problems.push(format!("  MISSING {} {}", kind, name)),
            Some(got) if got != want_sql => problems.push(format!(
                "  {} {} DIFFERS\n    want: {}\n    got:  {}",
                kind, name, want_sql, got
            )),
            _ => {}
        }
    }
    for name in actual.keys() {
        if !want.contains_key(name) && !OWN_ADDITIONS.contains(&name.as_str()) {
            problems.push(format!("  UNEXPECTED {} {}", kind, name));
        }
    }

    assert!(
        problems.is_empty(),
        "schema does not match the generated SQL:\n{}",
        problems.join("\n")
    );
}

/// A database holding exactly the shipped schema, built independently of the
/// code under test.
fn expected() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA_TABLES).unwrap();
    conn.execute_batch(SCHEMA_INDEXES).unwrap();
    conn
}

#[test]
fn every_table_matches_the_generated_schema() {
    let db = Database::open_in_memory().unwrap();
    assert_matches_schema(&db.conn(), "table");
}

#[test]
fn every_index_matches_the_generated_schema() {
    let db = Database::open_in_memory().unwrap();
    assert_matches_schema(&db.conn(), "index");
}

#[test]
fn a_database_has_no_triggers() {
    // The repositories keep derived data in step since schema v31; a trigger
    // would do the same work a second time.
    let db = Database::open_in_memory().unwrap();
    assert!(objects(&db.conn(), "trigger").is_empty());
}

#[test]
fn the_schema_carries_no_target_only_columns() {
    // The four tables that had drifted. Each assertion names a column that was
    // present here and absent upstream, or vice versa.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let cols = |t: &str| -> Vec<String> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", t)).unwrap();
        stmt.query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };

    let wiki = cols("wiki_pages");
    assert!(wiki.contains(&"summary".to_string()));
    assert!(wiki.contains(&"mtime".to_string()));
    assert!(
        !wiki.contains(&"topic".to_string()),
        "topic was target-only"
    );

    assert!(cols("entities").contains(&"node_id".to_string()));
    assert!(cols("memory_entities").contains(&"created_at".to_string()));

    let relations = cols("entity_relations");
    assert!(relations.contains(&"subject_entity_id".to_string()));
    assert!(relations.contains(&"relation".to_string()));
    assert!(
        !relations.contains(&"subject_id".to_string()),
        "subject_id was this crate's own name for it"
    );
}

#[test]
fn memory_entities_has_no_foreign_keys() {
    // The reference omits them deliberately: sync can deliver a mention link
    // before the memory it points at, and a cascade would reject that.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut stmt = conn
        .prepare("PRAGMA foreign_key_list(memory_entities)")
        .unwrap();
    let count = stmt.query_map([], |_| Ok(())).unwrap().count();
    assert_eq!(count, 0);
}

#[test]
fn the_version_stamp_matches_the_schema_present() {
    let db = Database::open_in_memory().unwrap();
    let version: i32 = db
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn reopening_changes_nothing() {
    let tmp = TempDb::new("idempotent");
    let snapshot = |path: &PathBuf| {
        let db = Database::open(path).unwrap();
        let conn = db.conn();
        (
            objects(&conn, "table"),
            objects(&conn, "index"),
            objects(&conn, "trigger"),
        )
    };
    assert_eq!(snapshot(&tmp.0), snapshot(&tmp.0));
}

#[test]
fn a_legacy_database_is_reconciled_to_the_generated_schema() {
    let tmp = TempDb::new("legacy");

    // Exactly what earlier versions of this crate wrote: the old shapes, with
    // last_accessed_at, wiki_pages.topic, cascading memory_entities, stamped 19.
    {
        let conn = Connection::open(&tmp.0).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE memories (
                id TEXT PRIMARY KEY, content TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'general',
                tags TEXT NOT NULL DEFAULT '[\"carried\"]',
                source TEXT NOT NULL DEFAULT 'manual',
                metadata TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                last_accessed_at TEXT NOT NULL
            );
            CREATE TABLE wiki_pages (
                slug TEXT PRIMARY KEY, title TEXT NOT NULL, content TEXT NOT NULL,
                topic TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE entities (
                id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, kind TEXT,
                aliases TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE memory_entities (
                memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                entity_id TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
                PRIMARY KEY (memory_id, entity_id)
            );
            INSERT INTO memories (id, content, created_at, updated_at, last_accessed_at)
            VALUES ('mem_old', 'survivor', '2020-06-15T12:00:00+00:00',
                    '2020-06-15T12:00:00+00:00', '2020-06-15T12:00:00+00:00');
            INSERT INTO wiki_pages VALUES
                ('page', 'Page', 'body', 'general',
                 '2020-06-15T12:00:00+00:00', '2020-06-15T12:00:00+00:00');
            PRAGMA user_version = 19;
            ",
        )
        .unwrap();
    }

    let db = Database::open(&tmp.0).unwrap();
    let conn = db.conn();

    assert_matches_schema(&conn, "table");
    assert_matches_schema(&conn, "index");
    assert_matches_schema(&conn, "trigger");

    // Data survives the rebuild.
    let content: String = conn
        .query_row("SELECT content FROM memories WHERE id='mem_old'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(content, "survivor");

    let title: String = conn
        .query_row("SELECT title FROM wiki_pages WHERE slug='page'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        title, "Page",
        "the wiki_pages rebuild must carry rows across"
    );

    // The rename preserves the value rather than resetting it.
    let accessed: String = conn
        .query_row(
            "SELECT accessed_at FROM memories WHERE id='mem_old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(accessed, "2020-06-15T12:00:00+00:00");

    // Derived tables are backfilled for rows that predate the triggers.
    let tags: i64 = conn
        .query_row(
            "SELECT count(*) FROM memory_tags WHERE memory_id='mem_old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tags, 1, "pre-existing JSON tags must be backfilled");
}

#[test]
fn writes_still_reach_the_sync_outbox() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var(NODE_ID_ENV, "node-a");
    crate::test_env::set_var(HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(SYNC_SECRET_ENV, "shh");

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    queries::add_memory(
        &conn,
        MemoryAddInput {
            sensitive: false,
            content: "syncable".into(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap();

    let payload: String = conn
        .query_row(
            "SELECT payload FROM sync_outbox ORDER BY id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(parsed["content"], "syncable");
    assert!(parsed.get("base_weight").is_some());

    crate::test_env::remove_var(NODE_ID_ENV);
    crate::test_env::remove_var(HUB_URL_ENV);
    crate::test_env::remove_var(SYNC_SECRET_ENV);
}

#[test]
fn writes_do_not_reach_the_outbox_while_sync_is_unconfigured() {
    // The `#76` regression case: memories_outbox_ai/au are gated on
    // sync_flags.sync_enabled, so a write on a node that has never
    // configured sync must not queue anything at all.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(NODE_ID_ENV);
    crate::test_env::remove_var(HUB_URL_ENV);
    crate::test_env::remove_var(SYNC_SECRET_ENV);

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    queries::add_memory(
        &conn,
        MemoryAddInput {
            sensitive: false,
            content: "not synced anywhere".into(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap();

    let count: i64 = conn
        .query_row("SELECT count(*) FROM sync_outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

// ---------------------------------------------------------------------------
// Embedding-model versioning at startup (#96)
// ---------------------------------------------------------------------------
//
// `Database::open`'s `initialize_schema` is where the reference's own
// "check at every startup" happens in this crate. These plant a stale
// `embedding_meta` row plus a fake stored vector directly via SQL (no real
// Ollama daemon needed) and reopen the same on-disk database to exercise the
// actual startup wiring, not just `vectors::reconcile_embedding_meta` in
// isolation.

fn plant_stale_vector(conn: &Connection, model: &str, dim: usize) {
    conn.execute(
        "INSERT INTO memories (id, content, created_at, updated_at) VALUES \
         ('mem_versioning', 'x', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO vec_chunks (memory_id, chunk_ix, embedding) VALUES ('mem_versioning', 0, ?)",
        [vec![0u8; dim * 4]],
    )
    .unwrap();
    for (key, value) in [("backend", "ollama"), ("model", model)] {
        conn.execute(
            "INSERT INTO embedding_meta (key, value, updated_at) VALUES (?, ?, '2020-01-01T00:00:00Z')",
            rusqlite::params![key, value],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO embedding_meta (key, value, updated_at) VALUES ('dim', ?, '2020-01-01T00:00:00Z')",
        rusqlite::params![dim.to_string()],
    )
    .unwrap();
}

fn stored_vector_counts(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM vec_chunks", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn reopening_with_an_unchanged_ollama_model_leaves_stored_vectors_alone() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("embedding_versioning_match");
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    crate::test_env::set_var(OLLAMA_MODEL_ENV, "nomic-embed-text");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");

    {
        let db = Database::open(&tmp.0).unwrap();
        plant_stale_vector(&db.conn(), "nomic-embed-text", 4);
    }

    // Reopening under the exact same configuration must not touch anything.
    let db = Database::open(&tmp.0).unwrap();
    assert_eq!(stored_vector_counts(&db.conn()), 1);

    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    crate::test_env::remove_var(OLLAMA_MODEL_ENV);
    crate::test_env::remove_var(EMBEDDING_DIM_ENV);
}

#[test]
fn reopening_with_a_changed_ollama_model_clears_stored_vectors() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("embedding_versioning_model_change");
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    crate::test_env::set_var(OLLAMA_MODEL_ENV, "old-model");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");

    {
        let db = Database::open(&tmp.0).unwrap();
        plant_stale_vector(&db.conn(), "old-model", 4);
    }

    crate::test_env::set_var(OLLAMA_MODEL_ENV, "new-model");
    let db = Database::open(&tmp.0).unwrap();
    assert_eq!(
        stored_vector_counts(&db.conn()),
        0,
        "a changed REMIND_ME_OLLAMA_EMBED_MODEL must clear the stale vector on open"
    );

    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    crate::test_env::remove_var(OLLAMA_MODEL_ENV);
    crate::test_env::remove_var(EMBEDDING_DIM_ENV);
}

#[test]
fn reopening_with_a_changed_embedding_dimension_clears_stored_vectors() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("embedding_versioning_dim_change");
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    crate::test_env::set_var(OLLAMA_MODEL_ENV, "nomic-embed-text");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");

    {
        let db = Database::open(&tmp.0).unwrap();
        plant_stale_vector(&db.conn(), "nomic-embed-text", 4);
    }

    crate::test_env::set_var(EMBEDDING_DIM_ENV, "8");
    let db = Database::open(&tmp.0).unwrap();
    assert_eq!(
        stored_vector_counts(&db.conn()),
        0,
        "a changed REMIND_ME_EMBEDDING_DIM must clear the stale vector on open"
    );

    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    crate::test_env::remove_var(OLLAMA_MODEL_ENV);
    crate::test_env::remove_var(EMBEDDING_DIM_ENV);
}

#[test]
fn a_first_ever_open_with_no_prior_embedding_meta_does_not_touch_vec_chunks() {
    // No embedding_meta recorded yet: nothing to compare against, so a fresh
    // database (or one predating this feature) must not spuriously clear
    // anything a still-earlier write may have put in vec_chunks directly
    // (as opposed to through embed_and_store, which would have recorded its
    // own identity already).
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("embedding_versioning_first_run");
    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    crate::test_env::remove_var(OLLAMA_MODEL_ENV);
    crate::test_env::remove_var(EMBEDDING_DIM_ENV);

    {
        let conn = Connection::open(&tmp.0).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE memories (id TEXT PRIMARY KEY, content TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
            INSERT INTO memories (id, content, created_at, updated_at) VALUES ('mem_x', 'x', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z');
            ",
        )
        .unwrap();
    }

    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    crate::test_env::set_var(OLLAMA_MODEL_ENV, "nomic-embed-text");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");
    // This open both creates vec_chunks for the first time
    // (they did not exist in the hand-built database above) and runs the
    // startup reconcile -- there is nothing in it to clear, and
    // no embedding_meta row to false-positive against.
    let db = Database::open(&tmp.0).unwrap();
    assert_eq!(stored_vector_counts(&db.conn()), 0);
    let meta_rows: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM embedding_meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        meta_rows, 0,
        "the startup check itself must not write embedding_meta -- only a real embed does"
    );

    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    crate::test_env::remove_var(OLLAMA_MODEL_ENV);
    crate::test_env::remove_var(EMBEDDING_DIM_ENV);
}

#[test]
fn reopening_with_the_embedding_backend_disabled_never_clears_stored_vectors() {
    // This crate's own adaptation (ADR-0002): unlike the reference, whose
    // ONNX backend is on by default, embeddings here are off unless
    // REMIND_ME_EMBEDDING_BACKEND=ollama is set. With it unset, nothing was
    // written by this process either, so the startup check is skipped
    // entirely rather than comparing against a meaningless "current"
    // identity.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDb::new("embedding_versioning_backend_disabled");
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    crate::test_env::set_var(OLLAMA_MODEL_ENV, "nomic-embed-text");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "4");

    {
        let db = Database::open(&tmp.0).unwrap();
        plant_stale_vector(&db.conn(), "nomic-embed-text", 4);
    }

    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    let db = Database::open(&tmp.0).unwrap();
    assert_eq!(stored_vector_counts(&db.conn()), 1);

    crate::test_env::remove_var(OLLAMA_MODEL_ENV);
    crate::test_env::remove_var(EMBEDDING_DIM_ENV);
}

// ---------------------------------------------------------------------------
// The v19 -> v27 step (issue #101)
// ---------------------------------------------------------------------------
//
// The whole-schema tests above compare the live database against the shipped
// `schema_*.sql`. That catches `migrations.rs` drifting from the SQL files, but
// not the SQL files drifting from `remind_me` — they would agree with each
// other just as happily at v19 as at v27.
//
// These name the eight migration steps' worth of objects explicitly, taken
// from the reference's own migration functions rather than from the dump, so
// regenerating against an older reference (or hand-editing one back out) fails
// here instead of silently shipping a schema that claims a version it does not
// have.

/// Every object `remind_me` added between v19 and v27, with the migration that
/// introduced it. Sourced from `db.py`'s `_migrate_vN_to_vN+1` functions.
const V27_TABLES: &[(&str, &str)] = &[
    ("reminder_deliveries", "_migrate_v22_to_v23"),
    ("memory_revisions", "_migrate_v23_to_v24"),
    ("analytics_snapshots", "_migrate_v24_to_v25"),
    ("saved_searches", "_migrate_v26_to_v27"),
    ("saved_search_seen_memories", "_migrate_v26_to_v27"),
];

const V27_INDEXES: &[(&str, &str)] = &[
    ("idx_memories_normalized_from", "_migrate_v20_to_v21"),
    ("idx_memories_remind_at", "_migrate_v22_to_v23"),
    (
        "idx_reminder_deliveries_memory_remind_at",
        "_migrate_v22_to_v23",
    ),
    ("idx_memory_revisions_memory_edited", "_migrate_v23_to_v24"),
    ("idx_analytics_snapshots_captured_at", "_migrate_v24_to_v25"),
    (
        "idx_saved_search_seen_memories_search_memory",
        "_migrate_v26_to_v27",
    ),
];

const V27_COLUMNS: &[(&str, &str, &str)] = &[
    ("sync_log", "last_pull_at", "_migrate_v19_to_v20"),
    ("sync_log", "last_push_at", "_migrate_v19_to_v20"),
    ("sync_log", "last_attempt_at", "_migrate_v19_to_v20"),
    ("memories", "remind_at", "_migrate_v22_to_v23"),
    ("memories", "sensitive", "_migrate_v25_to_v26"),
];

#[test]
fn the_schema_version_is_the_current_one() {
    // Written by hand, not derived from the schema files, so a schema change
    // that forgets to bump it fails here. 29 was the Python reference's last
    // (27 -> 29 covered its `sync_log.last_pull_seq` and the `reference`
    // refiling); 30 is this crate's own, vector chunks keyed by memory id
    // (ADR-0023).
    assert_eq!(SCHEMA_VERSION, 31);
}

#[test]
fn the_generated_schema_carries_every_v27_object() {
    // Still named v27 at schema 29 on purpose, not by oversight: the
    // reference's v28 adds a *column* (`sync_log.last_pull_seq`) and its v29
    // is data-only, so neither contributes a table or index this inventory
    // could list. The column itself is covered by the whole-DDL comparison in
    // `every_table_matches_the_generated_schema`.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let tables = objects(&conn, "table");
    let indexes = objects(&conn, "index");

    let mut missing = Vec::new();
    for (name, migration) in V27_TABLES {
        if !tables.contains_key(*name) {
            missing.push(format!("  table {} ({})", name, migration));
        }
    }
    for (name, migration) in V27_INDEXES {
        if !indexes.contains_key(*name) {
            missing.push(format!("  index {} ({})", name, migration));
        }
    }
    for (table, column, migration) in V27_COLUMNS {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({})", table))
            .unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        if !cols.contains(&column.to_string()) {
            missing.push(format!("  column {}.{} ({})", table, column, migration));
        }
    }

    assert!(
        missing.is_empty(),
        "the schema is stamped v{} but is missing objects the reference added \
         on the way there — regenerate from a current remind_me:\n{}",
        SCHEMA_VERSION,
        missing.join("\n")
    );
}

#[test]
fn the_outbox_payloads_carry_the_new_columns() {
    // Gap S10. A synced peer reconstructs a memory from the queued payload
    // alone, so a column that exists in `memories` but not in the payload is
    // silently dropped in transit: the failure is invisible locally and only
    // shows up as data loss on the other node.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute(
        "INSERT OR REPLACE INTO sync_flags (key, value) VALUES ('sync_enabled', '1')",
        [],
    )
    .unwrap();
    remind_me_core::db::memories::Memories::new(&conn)
        .insert(&remind_me_core::db::memories::NewMemory::new(
            "mem_s10",
            "x",
            "2026-01-01T00:00:00+00:00",
        ))
        .unwrap();
    let payload: String = conn
        .query_row("SELECT payload FROM sync_outbox", [], |r| r.get(0))
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    for column in ["remind_at", "sensitive"] {
        assert!(
            payload.get(column).is_some(),
            "the payload does not carry {column}; a synced peer would drop it"
        );
    }
}

#[test]
fn a_v19_database_with_rows_reconciles_to_the_current_version() {
    let tmp = TempDb::new("v19_to_v27");

    // A database written by this crate at v19: the shipped schema with
    // everything v20-v27 added taken back out — the tables *and* the columns,
    // since dropping only the tables would leave `reconcile_columns` with
    // nothing to do and the test would pass without exercising it.
    {
        let conn = Connection::open(&tmp.0).unwrap();
        conn.execute_batch(SCHEMA_TABLES).unwrap();
        conn.execute_batch(SCHEMA_INDEXES).unwrap();
        for (name, _) in V27_TABLES {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {};", name))
                .unwrap();
        }
        // Indexes before columns: SQLite refuses to drop a column an index
        // still references, and `idx_memories_remind_at` covers one of them.
        for (name, _) in V27_INDEXES {
            conn.execute_batch(&format!("DROP INDEX IF EXISTS {};", name))
                .unwrap();
        }
        for (table, column, _) in V27_COLUMNS {
            conn.execute_batch(&format!("ALTER TABLE {} DROP COLUMN {};", table, column))
                .unwrap();
        }
        // The v19 outbox triggers: the v30 ones with the two payload pairs
        // v23/v26 added taken back out. Installed *after* the column drops so
        // they reference only columns that exist at v19. Opening must drop
        // them: a trigger whose payload omits `remind_at`/`sensitive` drops
        // those fields silently in transit, and any trigger at all would queue
        // each edit a second time.
        let v19_triggers = V30_TRIGGERS
            .replace(", 'remind_at', NEW.remind_at", "")
            .replace(", 'sensitive', NEW.sensitive", "");
        assert!(
            !v19_triggers.contains("NEW.remind_at") && !v19_triggers.contains("NEW.sensitive"),
            "the payload-stripping replacements no longer match the generated \
             trigger text; update them or this fixture is not a v19 database"
        );
        conn.execute_batch(&v19_triggers).unwrap();

        conn.execute_batch(
            "
            INSERT INTO memories (id, content, created_at, updated_at)
            VALUES ('mem_v19', 'survivor', '2020-06-15T12:00:00+00:00',
                    '2020-06-15T12:00:00+00:00');
            PRAGMA user_version = 19;
            ",
        )
        .unwrap();
    }

    let db = Database::open(&tmp.0).unwrap();
    let conn = db.conn();

    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        version, SCHEMA_VERSION,
        "the stamp must advance with the schema"
    );

    let content: String = conn
        .query_row(
            "SELECT content FROM memories WHERE id = 'mem_v19'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(content, "survivor", "reconciliation must not drop rows");

    assert_matches_schema(&conn, "table");
    assert_matches_schema(&conn, "index");
    assert_matches_schema(&conn, "trigger");

    // Explicit rather than leaning on assert_matches_schema above: a stale
    // trigger keeps working, quietly shipping an incomplete payload to every
    // peer, so the damage lands on a different machine.
    assert!(
        objects(&conn, "trigger").is_empty(),
        "the v19 triggers survived reconciliation"
    );

    // #95's pre-migration snapshot guard has to still fire for this step in
    // particular: v19 -> v27 is the largest reconciliation this crate has ever
    // shipped, and it is exactly the transition a user would most want a
    // rollback from. The label carries the version read *before* migrating.
    let backups = list_backups(&tmp.0.parent().unwrap().join("backups")).unwrap();
    assert_eq!(backups.len(), 1, "the v19 -> v27 step must be snapshotted");
    assert!(
        backups[0].filename.contains("pre-migration-v19"),
        "snapshot should be labelled with the pre-migration version, got {}",
        backups[0].filename
    );
}
