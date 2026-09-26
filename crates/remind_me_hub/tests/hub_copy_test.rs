//! The copy tool (ADR-0021, phase 3): a SQLite hub copied onto the embedded
//! engine answers every read as the source did, `hub_seq` included, and
//! nothing the engine cannot store is copied silently.
//!
//! The SQLite store is gone, so each source hub is rebuilt from a dump the
//! store wrote before it went (`tests/fixtures/sqlite_hub*.sql`), and the
//! copy is held to that store's recorded answers. The Postgres source is
//! covered in `hub_postgres_copy_test.rs`, which needs a server.

// Each test crate uses part of the shared suite.
#[allow(dead_code)]
#[path = "suite/recorded.rs"]
mod recorded;

use remind_me_hub::import;
use remind_me_hub::record;
use remind_me_hub::store::multimodal::MultimodalHubStore;
use remind_me_hub::store::{HubStore, PullCursor, PullQuery};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A fresh scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "remind_me_hub_copy_{label}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A SQLite hub database at `path`, rebuilt from the recorded dump `dump`.
fn sqlite_hub(path: &Path, dump: &str) {
    let conn = rusqlite::Connection::open(path).expect("create a SQLite hub");
    conn.execute_batch(dump).expect("load the recorded hub");
}

/// The script's hub, as the SQLite store left it before compaction.
const SCRIPT_HUB: &str = include_str!("fixtures/sqlite_hub.sql");
/// Two memories, the newer compacted away: `hub_meta` holds 2, the rows 1.
const COMPACTED_HUB: &str = include_str!("fixtures/sqlite_hub_compacted.sql");
/// `ok`, a memory with a 65-byte id, and a link to that memory.
const INVALID_HUB: &str = include_str!("fixtures/sqlite_hub_invalid.sql");

fn memory(id: &str, updated: &str) -> record::Record {
    record::parse(&json!({
        "id": id,
        "content": format!("content of {id}"),
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": updated,
    }))
    .unwrap()
}

fn last_seq(store: &dyn HubStore) -> i64 {
    store
        .pull_memories(&PullQuery {
            cursor: PullCursor::Seq(0),
            exclude_node: None,
            full: true,
            limit: 500,
        })
        .unwrap()
        .iter()
        .filter_map(|m| m["hub_seq"].as_i64())
        .max()
        .unwrap_or(0)
}

fn copy(source: &Path, target: &Path) -> MultimodalHubStore {
    let snapshot = import::sqlite::read(source).expect("read the source");
    assert!(snapshot.validate().is_empty(), "{:?}", snapshot.validate());
    let store = MultimodalHubStore::create_from_snapshot(target, &snapshot).expect("copy");
    import::verify(&snapshot, &store).expect("the copy verifies");
    store
}

#[test]
fn a_copied_sqlite_hub_answers_every_read_as_the_source_did() {
    let dir = Scratch::new("recorded");
    sqlite_hub(&dir.path("hub.db"), SCRIPT_HUB);
    let expected = recorded::recorded();

    let engine = copy(&dir.path("hub.db"), &dir.path("engine"));
    // Every hub_seq and origin_node carried over, so every cursor a node
    // holds, and every exclude_node filter, means the same thing.
    recorded::assert_answers(&engine, &expected["after_pushes"], "after the copy");

    // The next write on the copy is numbered as the source would have
    // numbered it: nothing was compacted, so one above its highest row.
    let next = memory("after-copy", "2026-09-01T00:00:00Z");
    assert!(engine.apply_record(&next, Some("node-a")).unwrap());
    assert_eq!(
        last_seq(&engine),
        recorded::highest_seq(&expected["after_pushes"]) + 1
    );

    // And the copy survives a reopen as it was written.
    drop(engine);
    let reopened = MultimodalHubStore::open(&dir.path("engine")).unwrap();
    assert_eq!(
        last_seq(&reopened),
        recorded::highest_seq(&expected["after_pushes"]) + 1
    );
}

#[test]
fn a_copy_never_reissues_a_hub_seq_the_source_compacted_away() {
    // The SQLite store remembered the highest hub_seq it issued in
    // `hub_meta`, above every remaining row once compaction purged the
    // newest. The copy must start above that mark too, or a node whose
    // cursor sits on the purged number never pulls the copy's next write.
    let dir = Scratch::new("high_water");
    sqlite_hub(&dir.path("hub.db"), COMPACTED_HUB);

    let engine = copy(&dir.path("hub.db"), &dir.path("engine"));
    assert_eq!(last_seq(&engine), 1, "the row holding hub_seq 2 is gone");
    let next = memory("after-copy", "2026-09-01T00:00:00Z");
    assert!(engine.apply_record(&next, None).unwrap());
    assert_eq!(last_seq(&engine), 3, "the copy reissued a purged hub_seq");
}

#[test]
fn ids_the_engine_cannot_store_are_listed_and_never_copied_silently() {
    let dir = Scratch::new("invalid");
    sqlite_hub(&dir.path("hub.db"), INVALID_HUB);

    let snapshot = import::sqlite::read(&dir.path("hub.db")).unwrap();
    let rejected = snapshot.validate();
    let tables: Vec<&str> = rejected.iter().map(|r| r.table).collect();
    assert_eq!(tables, ["memories", "memory_entities"], "{rejected:?}");
    assert!(rejected[0].reason.contains("65 bytes"), "{}", rejected[0]);

    // Refused whole, with nothing written.
    let target = dir.path("engine");
    assert!(MultimodalHubStore::create_from_snapshot(&target, &snapshot).is_err());
    assert!(!target.exists(), "a refused copy writes nothing");

    // Dropped on request, and the rest copies and verifies.
    let (kept, dropped) = snapshot.without_rejected();
    assert_eq!(dropped.len(), 2);
    let store = MultimodalHubStore::create_from_snapshot(&target, &kept).unwrap();
    import::verify(&kept, &store).unwrap();
    assert_eq!(store.stats().unwrap().total, 1);
    assert_eq!(store.stats().unwrap().memory_entities, 0);
}

#[test]
fn reading_the_source_writes_nothing_to_it() {
    let dir = Scratch::new("read_only");
    let path = dir.path("hub.db");
    sqlite_hub(&path, SCRIPT_HUB);
    let before = std::fs::read(&path).unwrap();
    import::sqlite::read(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn a_copy_never_lands_in_a_directory_that_holds_anything() {
    let dir = Scratch::new("target");
    sqlite_hub(&dir.path("hub.db"), COMPACTED_HUB);
    let target = dir.path("engine");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("something"), b"x").unwrap();

    let err = MultimodalHubStore::check_copy_target(&target).unwrap_err();
    assert!(err.0.contains("not empty"), "{}", err.0);
    let snapshot = import::sqlite::read(&dir.path("hub.db")).unwrap();
    assert!(MultimodalHubStore::create_from_snapshot(&target, &snapshot).is_err());
}

/// The binary end to end: `--check` lists what cannot be copied and fails,
/// a plain copy refuses, `--drop-invalid` copies the rest and verifies.
#[test]
fn the_copy_tool_checks_refuses_and_copies() {
    let dir = Scratch::new("cli");
    sqlite_hub(&dir.path("hub.db"), INVALID_HUB);

    let tool = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rusty-remind-me-hub-copy"))
            .arg("--from-sqlite")
            .arg(dir.path("hub.db"))
            .arg("--to")
            .arg(dir.path("engine"))
            .args(extra)
            .output()
            .expect("run the copy tool")
    };

    let check = tool(&["--check"]);
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert!(!check.status.success(), "{stderr}");
    assert!(stderr.contains("cannot copy memories"), "{stderr}");
    assert!(!dir.path("engine").exists(), "--check writes nothing");

    let refused = tool(&[]);
    assert!(!refused.status.success());
    assert!(
        !dir.path("engine").exists(),
        "a refused copy writes nothing"
    );

    let copied = tool(&["--drop-invalid"]);
    let stderr = String::from_utf8_lossy(&copied.stderr);
    assert!(copied.status.success(), "{stderr}");
    assert!(stderr.contains("wrote and verified"), "{stderr}");

    let engine = MultimodalHubStore::open(&dir.path("engine")).unwrap();
    assert_eq!(
        engine.stats().unwrap().total,
        1,
        "everything but the over-long id"
    );
}
