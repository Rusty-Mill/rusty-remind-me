//! The embedded-engine backend against SQLite, with no server needed.
//!
//! The route suite (`hub_routes_test.rs`) already runs against this backend.
//! This adds the differential check (`suite/differential.rs`): one script of
//! pushes, then every read compared with SQLite's answer. `hub_postgres_test`
//! runs the same check with Postgres as a third backend.
#![cfg(feature = "multimodal-store")]

// Each test crate uses part of the shared suite.
#[allow(dead_code)]
#[path = "suite/differential.rs"]
mod differential;

use remind_me_hub::store::multimodal::MultimodalHubStore;
use remind_me_hub::store::sqlite::SqliteStore;
use remind_me_hub::store::HubStore;

#[test]
fn the_engine_answers_every_read_as_sqlite_does() {
    let dir =
        std::env::temp_dir().join(format!("remind_me_hub_differential_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let lite = SqliteStore::open_in_memory().expect("sqlite");
    lite.migrate().expect("migrate sqlite");
    let engine = MultimodalHubStore::open(&dir).expect("open the engine store");
    engine.migrate().expect("migrate the engine store");

    differential::assert_backends_agree(
        &[("sqlite", &lite), ("multimodal", &engine)],
        differential::SeqComparison::Exact,
    );

    // And the engine answers the same after a reopen, which rebuilds its
    // sort indexes from disk.
    let before = engine.stats().expect("stats");
    drop(engine);
    let reopened = MultimodalHubStore::open(&dir).expect("reopen");
    assert_eq!(reopened.stats().expect("stats"), before);
    assert_eq!(
        reopened.count_by_origin_node(None).unwrap(),
        lite.count_by_origin_node(None).unwrap()
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&dir);
}
