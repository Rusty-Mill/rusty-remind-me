//! The Postgres backend, against a real server.
//!
//! Skipped unless `REMIND_ME_HUB_TEST_DATABASE_URL` is set, because a database
//! server is not something a `cargo test` may assume.
//!
//! Be clear-eyed about what that costs: a skipped test here reports as
//! **passed**, and cargo captures the `SKIP` line unless you pass
//! `--nocapture`. So a local run that never touched a database looks exactly
//! like one that did. That is tolerable for a developer and intolerable for
//! CI, which is the run everyone actually trusts — so
//! `REMIND_ME_HUB_REQUIRE_POSTGRES=1` turns the skip into a hard failure, and
//! CI sets it. The environment cannot quietly lose its database and stay
//! green.
//!
//! # What is worth testing here specifically
//!
//! The route tests already cover the protocol against SQLite, and the trait
//! means both backends answer the same calls. So these do not re-test the
//! protocol. They cover what is *only* true of Postgres:
//!
//! - the legacy TIMESTAMPTZ→TEXT migration, which is the whole "drop-in"
//!   claim and cannot be exercised anywhere else,
//! - `nextval()`-driven `hub_seq`, including that concurrent pushes cannot
//!   commit out of sequence order and make a `Seq`-cursor puller skip a row,
//! - planner estimates, which SQLite answers `None` to,
//! - and a differential check that every backend agrees, which is the only
//!   thing that makes the trait more than a hopeful interface.
#![cfg(feature = "postgres-store")]

use remind_me_hub::record;
use remind_me_hub::store::postgres::PostgresStore;
use remind_me_hub::store::sqlite::SqliteStore;
use remind_me_hub::store::{HubStore, PullCursor, PullQuery, COUNTABLE};
use serde_json::{json, Value};

// Each test crate uses part of the shared suite.
#[allow(dead_code)]
#[path = "suite/differential.rs"]
mod differential;

/// Each test gets its own schema-clean database via a unique table prefix is
/// not possible here, so instead every test drops and recreates the tables it
/// uses. Serialised by a mutex because they share one database.
static DB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn url() -> Option<String> {
    let configured = std::env::var("REMIND_ME_HUB_TEST_DATABASE_URL")
        .ok()
        .filter(|s| !s.is_empty());
    if configured.is_none() && std::env::var("REMIND_ME_HUB_REQUIRE_POSTGRES").is_ok() {
        panic!(
            "REMIND_ME_HUB_REQUIRE_POSTGRES is set but \
             REMIND_ME_HUB_TEST_DATABASE_URL is not -- refusing to skip. \
             This exists so CI cannot lose its database and still report green."
        );
    }
    configured
}

fn reset(url: &str) {
    let mut client = postgres::Client::connect(url, postgres::NoTls).expect("connect to reset");
    client
        .batch_execute(
            "DROP TABLE IF EXISTS memories, entities, memory_entities, entity_relations CASCADE; \
             DROP SEQUENCE IF EXISTS memories_hub_seq CASCADE;",
        )
        .expect("drop the schema");
}

fn store(url: &str) -> PostgresStore {
    reset(url);
    let store = PostgresStore::new(url);
    store.migrate().expect("migrate");
    store
}

fn memory(id: &str, updated: &str) -> Value {
    json!({
        "id": id,
        "content": format!("content of {id}"),
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": updated,
    })
}

fn apply(store: &dyn HubStore, raw: &Value, origin: &str) -> bool {
    let parsed = record::parse(raw).expect("a well-formed record");
    store.apply_record(&parsed, Some(origin)).expect("apply")
}

fn pull_all(store: &dyn HubStore) -> Vec<Value> {
    store
        .pull_memories(&PullQuery {
            cursor: PullCursor::Since(remind_me_hub::EPOCH.to_string()),
            exclude_node: None,
            full: false,
            limit: 500,
        })
        .expect("pull")
}

#[test]
fn postgres_round_trips_a_record_and_applies_lww() {
    let Some(url) = url() else {
        eprintln!("SKIP: REMIND_ME_HUB_TEST_DATABASE_URL is not set");
        return;
    };
    let _guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = store(&url);

    assert!(apply(
        &store,
        &memory("m1", "2026-08-05T12:00:00Z"),
        "node-a"
    ));
    assert!(
        !apply(&store, &memory("m1", "2026-08-05T10:00:00Z"), "node-b"),
        "an older record must lose LWW"
    );
    assert!(apply(
        &store,
        &memory("m1", "2026-08-05T14:00:00Z"),
        "node-b"
    ));

    let records = pull_all(&store);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["updated_at"], "2026-08-05T14:00:00+00:00");
    assert!(
        records[0].get("origin_node").is_none(),
        "origin_node must never reach the wire"
    );
}

#[test]
fn hub_seq_advances_on_every_write_regardless_of_updated_at() {
    let Some(url) = url() else {
        eprintln!("SKIP: REMIND_ME_HUB_TEST_DATABASE_URL is not set");
        return;
    };
    let _guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = store(&url);

    apply(&store, &memory("a", "2026-08-05T10:00:00Z"), "node-a");
    let first = pull_all(&store)[0]["hub_seq"].as_i64().expect("a hub_seq");

    // Authored before `a`, pushed after it. The sequence must still advance.
    apply(&store, &memory("b", "2026-07-01T09:00:00Z"), "node-b");
    let by_seq = store
        .pull_memories(&PullQuery {
            cursor: PullCursor::Seq(first),
            exclude_node: None,
            full: false,
            limit: 500,
        })
        .expect("pull by seq");
    assert_eq!(by_seq.len(), 1, "the straggler must be visible by seq");
    assert_eq!(by_seq[0]["id"], "b");
}

#[test]
fn planner_estimates_are_available_on_postgres_unlike_sqlite() {
    let Some(url) = url() else {
        eprintln!("SKIP: REMIND_ME_HUB_TEST_DATABASE_URL is not set");
        return;
    };
    let _guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = store(&url);
    apply(&store, &memory("m1", "2026-08-05T10:00:00Z"), "node-a");

    let approx = store
        .approx_count_tables(&COUNTABLE)
        .expect("approx counts");
    assert!(
        approx.is_some(),
        "Postgres must offer an estimate; SQLite is the backend that returns None"
    );
    let approx = approx.unwrap();
    // The value is a planner estimate and may be 0 before ANALYZE -- the
    // contract is "fast and honestly approximate", so the assertion is about
    // the shape, not the number.
    assert!(approx.memories.is_some());
    assert!(
        approx.memories.unwrap().live.is_none(),
        "an estimate has no live/tombstone split to report"
    );
}

/// The drop-in claim, and the only place it can be tested.
#[test]
fn a_legacy_timestamptz_database_is_migrated_in_place() {
    let Some(url) = url() else {
        eprintln!("SKIP: REMIND_ME_HUB_TEST_DATABASE_URL is not set");
        return;
    };
    let _guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset(&url);

    // The legacy hub's schema: 11 columns, TIMESTAMPTZ timestamps.
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
    client
        .batch_execute(
            "CREATE TABLE memories (
                 id         TEXT PRIMARY KEY,
                 content    TEXT NOT NULL,
                 category   TEXT NOT NULL DEFAULT 'general',
                 tags       JSONB NOT NULL DEFAULT '[]',
                 source     TEXT NOT NULL DEFAULT 'manual',
                 metadata   JSONB NOT NULL DEFAULT '{}',
                 created_at TIMESTAMPTZ NOT NULL,
                 updated_at TIMESTAMPTZ NOT NULL,
                 capture_id TEXT,
                 node_id    TEXT,
                 client     TEXT NOT NULL DEFAULT 'unknown'
             );
             INSERT INTO memories (id, content, created_at, updated_at)
             VALUES ('legacy-1', 'from the old hub',
                     '2026-08-05 10:00:00+00', '2026-08-05 11:30:00+00'),
                    ('legacy-2', 'also old',
                     '2026-08-04 08:00:00+00', '2026-08-04 09:00:00.500000+00');",
        )
        .expect("create the legacy schema");
    drop(client);

    let store = PostgresStore::new(&url);
    store.migrate().expect("migrate a legacy database");

    let records = pull_all(&store);
    assert_eq!(records.len(), 2, "legacy rows must survive the migration");

    let by_id: std::collections::BTreeMap<&str, &Value> = records
        .iter()
        .map(|r| (r["id"].as_str().unwrap(), r))
        .collect();

    // Timestamps became canonical TEXT, matching what a client would have
    // written -- no fractional part when zero, six digits when not.
    assert_eq!(by_id["legacy-1"]["updated_at"], "2026-08-05T11:30:00+00:00");
    assert_eq!(
        by_id["legacy-2"]["updated_at"],
        "2026-08-04T09:00:00.500000+00:00"
    );

    // Columns added since the legacy schema carry client-matching defaults.
    assert_eq!(by_id["legacy-1"]["status"], "active");
    assert_eq!(by_id["legacy-1"]["memory_type"], "unclassified");
    assert_eq!(by_id["legacy-1"]["vitality"], 1.0);
    // accessed_at is backfilled from created_at rather than left null.
    assert_eq!(
        by_id["legacy-1"]["accessed_at"],
        "2026-08-05T10:00:00+00:00"
    );
    // hub_seq is backfilled in (updated_at, id) order, so the older record
    // sorts first and the migration does not itself reorder history.
    let seq1 = by_id["legacy-1"]["hub_seq"].as_i64().unwrap();
    let seq2 = by_id["legacy-2"]["hub_seq"].as_i64().unwrap();
    assert!(
        seq2 < seq1,
        "legacy-2 is older by updated_at so it should hold the lower seq \
         (got legacy-1={seq1}, legacy-2={seq2})"
    );

    // And the migrated database still works.
    assert!(apply(
        &store,
        &memory("new", "2026-08-06T00:00:00Z"),
        "node-a"
    ));
    assert_eq!(pull_all(&store).len(), 3);
}

#[test]
fn migrate_is_idempotent() {
    let Some(url) = url() else {
        eprintln!("SKIP: REMIND_ME_HUB_TEST_DATABASE_URL is not set");
        return;
    };
    let _guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = store(&url);
    apply(&store, &memory("m1", "2026-08-05T10:00:00Z"), "node-a");
    let before = pull_all(&store)[0]["hub_seq"].as_i64().unwrap();

    // A restart re-runs migrate; it must not renumber or duplicate anything.
    store.migrate().expect("second migrate");
    store.migrate().expect("third migrate");

    let records = pull_all(&store);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["hub_seq"].as_i64().unwrap(),
        before,
        "re-running migrate must not renumber existing rows"
    );
}

/// The check that makes the trait more than a hopeful interface: the
/// shared differential script (`suite/differential.rs`) against Postgres,
/// SQLite, and the embedded engine when the `multimodal-store` feature
/// builds it.
#[test]
fn every_backend_answers_the_same_protocol_identically() {
    let Some(url) = url() else {
        eprintln!("SKIP: REMIND_ME_HUB_TEST_DATABASE_URL is not set");
        return;
    };
    let _guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let pg = store(&url);
    let lite = SqliteStore::open_in_memory().expect("sqlite");
    lite.migrate().expect("migrate sqlite");
    #[cfg_attr(not(feature = "multimodal-store"), allow(unused_mut))]
    let mut backends: Vec<(&str, &dyn HubStore)> = vec![("postgres", &pg), ("sqlite", &lite)];

    #[cfg(feature = "multimodal-store")]
    let engine_dir = std::env::temp_dir().join(format!(
        "remind_me_hub_pg_differential_{}",
        std::process::id()
    ));
    #[cfg(feature = "multimodal-store")]
    let engine = {
        let _ = std::fs::remove_dir_all(&engine_dir);
        remind_me_hub::store::multimodal::MultimodalHubStore::open(&engine_dir)
            .expect("open the engine store")
    };
    #[cfg(feature = "multimodal-store")]
    backends.push(("multimodal", &engine));

    // Ranked: Postgres's sequence has gaps where the others have none (see
    // `SeqComparison`); `hub_multimodal_test` compares the other two exactly.
    differential::assert_backends_agree(&backends, differential::SeqComparison::Ranked);

    #[cfg(feature = "multimodal-store")]
    {
        drop(backends);
        drop(engine);
        let _ = std::fs::remove_dir_all(&engine_dir);
    }
}

/// Concurrent pushes must never let a puller on the `Seq` cursor skip a row.
///
/// `nextval()` hands out `hub_seq` at statement time, but a row becomes
/// visible at commit. Without the advisory lock in `apply_record`, a push
/// holding seq 11 could commit after one holding seq 12; a puller that had
/// already read 12 would move its cursor past 11 and never see that row. This
/// ran 1200 pushes on 8 threads and skipped 36-40 rows on every run before
/// the fix. The SQLite backend serialises writes and never skipped any.
#[test]
fn a_seq_puller_sees_every_row_under_concurrent_pushes() {
    let Some(url) = url() else {
        eprintln!("SKIP: REMIND_ME_HUB_TEST_DATABASE_URL is not set");
        return;
    };
    let _guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = std::sync::Arc::new(store(&url));
    const WRITERS: usize = 8;
    const PER_WRITER: usize = 150;
    let pushing_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let puller = {
        let store = std::sync::Arc::clone(&store);
        let pushing_done = std::sync::Arc::clone(&pushing_done);
        std::thread::spawn(move || {
            let mut seen = std::collections::HashSet::new();
            let mut cursor = 0i64;
            loop {
                // Read the flag before draining, so the drain after the last
                // push has finished is always a complete one.
                let finished = pushing_done.load(std::sync::atomic::Ordering::SeqCst);
                loop {
                    let page = store
                        .pull_memories(&PullQuery {
                            cursor: PullCursor::Seq(cursor),
                            exclude_node: None,
                            full: false,
                            limit: 50,
                        })
                        .expect("pull");
                    if page.is_empty() {
                        break;
                    }
                    for row in &page {
                        seen.insert(row["id"].as_str().expect("id").to_string());
                        cursor = cursor.max(row["hub_seq"].as_i64().expect("hub_seq"));
                    }
                }
                if finished {
                    return seen;
                }
            }
        })
    };

    let pushers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let store = std::sync::Arc::clone(&store);
            std::thread::spawn(move || {
                for i in 0..PER_WRITER {
                    apply(
                        store.as_ref(),
                        &memory(&format!("m-{w}-{i}"), "2026-08-05T12:00:00Z"),
                        "node-a",
                    );
                }
            })
        })
        .collect();
    for pusher in pushers {
        pusher.join().expect("pusher thread");
    }
    pushing_done.store(true, std::sync::atomic::Ordering::SeqCst);
    let seen = puller.join().expect("puller thread");

    let missed: Vec<String> = (0..WRITERS)
        .flat_map(|w| (0..PER_WRITER).map(move |i| format!("m-{w}-{i}")))
        .filter(|id| !seen.contains(id))
        .collect();
    assert!(
        missed.is_empty(),
        "a Seq-cursor puller skipped {} of {} rows, e.g. {:?}",
        missed.len(),
        WRITERS * PER_WRITER,
        &missed[..missed.len().min(5)]
    );
}

/// The copy tool's Postgres reader (ADR-0021, phase 3), against a hub with
/// `hub_seq` gaps: every LWW loss spends a `nextval()`. The copy keeps every
/// number exactly, and issues the next one above the sequence's high-water
/// mark, not just above the rows, as Postgres itself would.
#[cfg(feature = "postgres-import")]
#[test]
fn a_postgres_hub_copies_onto_the_engine_with_every_hub_seq_kept() {
    let Some(url) = url() else {
        eprintln!("SKIP: REMIND_ME_HUB_TEST_DATABASE_URL is not set");
        return;
    };
    let _guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let pg = store(&url);
    differential::apply_script(&pg);
    // One more LWW loss, so the sequence ends above every row's hub_seq.
    assert!(!apply(&pg, &memory("m1", "2026-01-01T00:00:00Z"), "node-b"));

    let dir = std::env::temp_dir().join(format!("remind_me_hub_pg_copy_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let snapshot = remind_me_hub::import::postgres::read(&url).expect("read the Postgres hub");
    assert!(snapshot.validate().is_empty(), "{:?}", snapshot.validate());
    let engine =
        remind_me_hub::store::multimodal::MultimodalHubStore::create_from_snapshot(&dir, &snapshot)
            .expect("copy");
    remind_me_hub::import::verify(&snapshot, &engine).expect("the copy verifies");

    differential::assert_answers_agree(
        &[("postgres", &pg), ("copy", &engine)],
        differential::SeqComparison::Exact,
        "after the copy",
    );

    // The next write gets the same hub_seq on both: the copy starts above
    // the sequence's last value, gaps and all.
    let next = memory("after-copy", "2026-09-01T00:00:00Z");
    assert!(apply(&pg, &next, "node-a"));
    assert!(apply(&engine, &next, "node-a"));
    let seq_of = |store: &dyn HubStore| {
        pull_all(store)
            .iter()
            .find(|m| m["id"] == "after-copy")
            .and_then(|m| m["hub_seq"].as_i64())
            .unwrap()
    };
    assert_eq!(seq_of(&engine), seq_of(&pg));

    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A legacy database the Python hub left behind reads without being
/// migrated, and copies to what the Postgres store's own in-place
/// migration makes of it.
#[cfg(feature = "postgres-import")]
#[test]
fn a_legacy_postgres_hub_copies_without_being_migrated() {
    let Some(url) = url() else {
        eprintln!("SKIP: REMIND_ME_HUB_TEST_DATABASE_URL is not set");
        return;
    };
    let _guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset(&url);
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
    client
        .batch_execute(
            "CREATE TABLE memories (
                 id         TEXT PRIMARY KEY,
                 content    TEXT NOT NULL,
                 category   TEXT NOT NULL DEFAULT 'general',
                 tags       JSONB NOT NULL DEFAULT '[]',
                 source     TEXT NOT NULL DEFAULT 'manual',
                 metadata   JSONB NOT NULL DEFAULT '{}',
                 created_at TIMESTAMPTZ NOT NULL,
                 updated_at TIMESTAMPTZ NOT NULL,
                 capture_id TEXT,
                 node_id    TEXT,
                 client     TEXT NOT NULL DEFAULT 'unknown'
             );
             INSERT INTO memories (id, content, tags, created_at, updated_at)
             VALUES ('legacy-1', 'from the old hub', '[\"a\"]',
                     '2026-08-05 10:00:00+00', '2026-08-05 11:30:00+00'),
                    ('legacy-2', 'also old', '[]',
                     '2026-08-04 08:00:00+00', '2026-08-04 09:00:00.500000+00');",
        )
        .expect("create the legacy schema");

    let snapshot = remind_me_hub::import::postgres::read(&url).expect("read the legacy hub");
    assert_eq!(snapshot.memories.len(), 2);
    let column_type: String = client
        .query_one(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 'memories' AND column_name = 'updated_at'",
            &[],
        )
        .unwrap()
        .get(0);
    assert!(
        column_type.starts_with("timestamp"),
        "reading must not migrate the source (updated_at is now {column_type})"
    );
    drop(client);

    let dir =
        std::env::temp_dir().join(format!("remind_me_hub_legacy_copy_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine =
        remind_me_hub::store::multimodal::MultimodalHubStore::create_from_snapshot(&dir, &snapshot)
            .expect("copy");

    // Now let the Postgres store migrate the source in place, and require
    // the copy to match it field for field, backfilled hub_seq included.
    let pg = PostgresStore::new(&url);
    pg.migrate().expect("migrate the legacy database");
    assert_eq!(pull_all(&engine), pull_all(&pg));

    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
}
