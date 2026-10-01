//! The copy tool's Postgres reader (ADR-0021, phase 3), against a real
//! server.
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
//! The Postgres store is gone, so each source hub is loaded from what it
//! left behind: a `pg_dump` of a hub it served (`fixtures/postgres_hub.sql`),
//! and the Python hub's legacy schema (`fixtures/legacy_postgres_hub.sql`).
//! Each copy is held to the answers the store gave, recorded before it went.
#![cfg(feature = "postgres-import")]

// Each test crate uses part of the shared suite.
#[allow(dead_code)]
#[path = "suite/recorded.rs"]
mod recorded;

use remind_me_hub::record;
use remind_me_hub::store::multimodal::MultimodalHubStore;
use remind_me_hub::store::{HubStore, PullCursor, PullQuery};
use serde_json::{json, Value};

/// Every test replaces the whole schema, so they share one database one at a
/// time.
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

/// Replace the test database's hub tables with the recorded hub `sql`.
fn load(url: &str, sql: &str) -> postgres::Client {
    let mut client = postgres::Client::connect(url, postgres::NoTls).expect("connect");
    client
        .batch_execute(
            "DROP TABLE IF EXISTS public.memories, public.entities, \
                 public.memory_entities, public.entity_relations CASCADE; \
             DROP SEQUENCE IF EXISTS public.memories_hub_seq CASCADE;",
        )
        .expect("drop the schema");
    client.batch_execute(sql).expect("load the recorded hub");
    // The dump empties `search_path` for its own statements; put it back.
    client
        .batch_execute("SET search_path TO public")
        .expect("reset search_path");
    client
}

fn scratch(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "remind_me_hub_pg_copy_{label}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
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

fn recorded_json(text: &str) -> Value {
    serde_json::from_str(text).expect("recorded JSON")
}

/// A hub with `hub_seq` gaps: every LWW loss spent a `nextval()`, the last
/// one after the newest row. The copy keeps every number exactly, and issues
/// the next one above the sequence's high-water mark, not just above the
/// rows, as the Postgres store did.
#[test]
fn a_postgres_hub_copies_onto_the_engine_with_every_hub_seq_kept() {
    let Some(url) = url() else {
        eprintln!("SKIP: REMIND_ME_HUB_TEST_DATABASE_URL is not set");
        return;
    };
    let _guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    load(&url, include_str!("fixtures/postgres_hub.sql"));
    let expected = recorded_json(include_str!("fixtures/postgres_answers.json"));

    let dir = scratch("gaps");
    let snapshot = remind_me_hub::import::postgres::read(&url).expect("read the Postgres hub");
    assert!(snapshot.validate().is_empty(), "{:?}", snapshot.validate());
    let engine = MultimodalHubStore::create_from_snapshot(&dir, &snapshot).expect("copy");
    remind_me_hub::import::verify(&snapshot, &engine).expect("the copy verifies");
    recorded::assert_answers(&engine, &expected["answers"], "after the copy");

    // The next write gets the hub_seq the Postgres store gave it.
    let next = record::parse(&json!({
        "id": "after-copy",
        "content": "content of after-copy",
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": "2026-09-01T00:00:00Z",
    }))
    .unwrap();
    assert!(engine.apply_record(&next, Some("node-a")).unwrap());
    let seq = pull_all(&engine)
        .into_iter()
        .find(|m| m["id"] == "after-copy")
        .and_then(|m| m["hub_seq"].as_i64());
    assert_eq!(seq, expected["next_hub_seq"].as_i64());

    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A legacy database the Python hub left behind reads without being
/// migrated, and copies to what the Postgres store's in-place migration made
/// of it, backfilled `hub_seq` included.
#[test]
fn a_legacy_postgres_hub_copies_without_being_migrated() {
    let Some(url) = url() else {
        eprintln!("SKIP: REMIND_ME_HUB_TEST_DATABASE_URL is not set");
        return;
    };
    let _guard = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut client = load(&url, include_str!("fixtures/legacy_postgres_hub.sql"));

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

    let dir = scratch("legacy");
    let engine = MultimodalHubStore::create_from_snapshot(&dir, &snapshot).expect("copy");
    let migrated = recorded_json(include_str!("fixtures/legacy_postgres_migrated.json"));
    assert_eq!(json!(pull_all(&engine)), migrated);

    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
}
