//! The engine store against the recorded answers of the SQL stores it
//! replaced, and under concurrent pushes.
//!
//! The route suite (`hub_routes_test.rs`) covers the protocol request by
//! request. This adds the recorded check (`suite/recorded.rs`): one script
//! of pushes, then every read compared with what the retired SQLite store
//! answered, before and after a tombstone compaction and after a reopen.

// Each test crate uses part of the shared suite.
#[allow(dead_code)]
#[path = "suite/recorded.rs"]
mod recorded;

use remind_me_hub::record;
use remind_me_hub::store::multimodal::MultimodalHubStore;
use remind_me_hub::store::{HubStore, PullCursor, PullQuery};
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "remind_me_hub_engine_{label}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn the_engine_answers_every_read_as_the_sql_stores_did() {
    let dir = scratch("recorded");
    let expected = recorded::recorded();
    let engine = MultimodalHubStore::open(&dir).expect("open the engine store");

    let applied = recorded::apply_script(&engine);
    assert_eq!(json!(applied), expected["applied"], "which pushes applied");
    recorded::assert_answers(&engine, &expected["after_pushes"], "after the pushes");

    assert_eq!(
        engine
            .compact_tombstones(recorded::COMPACT_CUTOFF)
            .expect("compact"),
        1,
        "the script holds one expired tombstone"
    );
    recorded::assert_answers(
        &engine,
        &expected["after_compaction"],
        "after compacting tombstones",
    );

    // And the same after a reopen, which rebuilds the sort indexes from disk.
    drop(engine);
    let reopened = MultimodalHubStore::open(&dir).expect("reopen");
    recorded::assert_answers(&reopened, &expected["after_compaction"], "after a reopen");
    drop(reopened);
    let _ = std::fs::remove_dir_all(&dir);
}

fn memory(id: &str) -> record::Record {
    record::parse(&json!({
        "id": id,
        "content": format!("content of {id}"),
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": "2026-08-05T12:00:00Z",
    }))
    .expect("a well-formed record")
}

/// Drain every memory above `cursor`, returning the new cursor.
fn drain(store: &dyn HubStore, mut cursor: i64, seen: &mut HashSet<String>) -> i64 {
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
            return cursor;
        }
        for row in &page {
            seen.insert(row["id"].as_str().expect("id").to_string());
            cursor = cursor.max(row["hub_seq"].as_i64().expect("hub_seq"));
        }
    }
}

/// A node resumes strictly after the last `hub_seq` it pulled, so a row
/// that became visible below a cursor already handed out would never be
/// pulled. Postgres had exactly that bug (a later `nextval` committing
/// first); the engine assigns and publishes a `hub_seq` under one lock,
/// and this holds it to that.
#[test]
fn a_seq_puller_sees_every_row_under_concurrent_pushes() {
    const WRITERS: usize = 8;
    const PER_WRITER: usize = 150;
    let dir = scratch("concurrent");
    let store = Arc::new(MultimodalHubStore::open(&dir).expect("open the engine store"));
    let pushing_done = Arc::new(AtomicBool::new(false));

    let puller = {
        let store = Arc::clone(&store);
        let pushing_done = Arc::clone(&pushing_done);
        std::thread::spawn(move || {
            let mut seen = HashSet::new();
            let mut cursor = 0;
            loop {
                // Read the flag before draining, so the drain after the last
                // push has finished is always a complete one.
                let finished = pushing_done.load(Ordering::SeqCst);
                cursor = drain(store.as_ref(), cursor, &mut seen);
                if finished {
                    return seen;
                }
            }
        })
    };

    let pushers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                for i in 0..PER_WRITER {
                    let applied = store
                        .apply_record(&memory(&format!("m-{w}-{i}")), Some("node-a"))
                        .expect("apply");
                    assert!(applied);
                }
            })
        })
        .collect();
    for pusher in pushers {
        pusher.join().expect("pusher thread");
    }
    pushing_done.store(true, Ordering::SeqCst);
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
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
