//! The copy tool (ADR-0021, phase 3): a SQLite hub copied onto the embedded
//! engine answers every read as the source did, `hub_seq` included, and
//! nothing the engine cannot store is copied silently. The Postgres source
//! is covered in `hub_postgres_test.rs`, which needs a server.
#![cfg(feature = "multimodal-store")]

// Each test crate uses part of the shared suite.
#[allow(dead_code)]
#[path = "suite/differential.rs"]
mod differential;

use remind_me_hub::import;
use remind_me_hub::record;
use remind_me_hub::store::multimodal::MultimodalHubStore;
use remind_me_hub::store::sqlite::SqliteStore;
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

fn sqlite_hub(path: &Path) -> SqliteStore {
    let store = SqliteStore::open(path.to_str().unwrap()).expect("open a SQLite hub");
    store.migrate().expect("migrate");
    store
}

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
    let dir = Scratch::new("differential");
    let lite = sqlite_hub(&dir.path("hub.db"));
    differential::apply_script(&lite);

    let engine = copy(&dir.path("hub.db"), &dir.path("engine"));
    // Exact: every hub_seq and origin_node carried over, so every cursor a
    // node holds, and every exclude_node filter, means the same thing.
    differential::assert_answers_agree(
        &[("sqlite", &lite), ("copy", &engine)],
        differential::SeqComparison::Exact,
        "after the copy",
    );

    // The next write on the copy is numbered as it would have been on the
    // source, so a node's cursor carries straight on.
    let next = memory("after-copy", "2026-09-01T00:00:00Z");
    assert!(lite.apply_record(&next, Some("node-a")).unwrap());
    assert!(engine.apply_record(&next, Some("node-a")).unwrap());
    assert_eq!(last_seq(&engine), last_seq(&lite));

    // And the copy survives a reopen as it was written.
    drop(engine);
    let reopened = MultimodalHubStore::open(&dir.path("engine")).unwrap();
    differential::assert_answers_agree(
        &[("sqlite", &lite), ("reopened copy", &reopened)],
        differential::SeqComparison::Exact,
        "after a reopen",
    );
}

#[test]
fn ids_the_engine_cannot_store_are_listed_and_never_copied_silently() {
    let dir = Scratch::new("invalid");
    let lite = sqlite_hub(&dir.path("hub.db"));
    let long = "x".repeat(65);
    lite.apply_record(&memory("ok", "2026-08-02T00:00:00Z"), None)
        .unwrap();
    lite.apply_record(&memory(&long, "2026-08-02T00:00:00Z"), None)
        .unwrap();
    let link = record::parse(&json!({
        "record_type": "memory_entity", "memory_id": long, "entity_id": "e1",
        "created_at": "2026-08-02T00:00:00Z",
    }))
    .unwrap();
    lite.apply_record(&link, None).unwrap();

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
    {
        let lite = sqlite_hub(&path);
        differential::apply_script(&lite);
    }
    let before = std::fs::read(&path).unwrap();
    import::sqlite::read(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn a_copy_never_lands_in_a_directory_that_holds_anything() {
    let dir = Scratch::new("target");
    let lite = sqlite_hub(&dir.path("hub.db"));
    lite.apply_record(&memory("m1", "2026-08-02T00:00:00Z"), None)
        .unwrap();
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
    let lite = sqlite_hub(&dir.path("hub.db"));
    differential::apply_script(&lite);
    lite.apply_record(&memory(&"y".repeat(70), "2026-08-02T00:00:00Z"), None)
        .unwrap();
    drop(lite);

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

    let lite = sqlite_hub(&dir.path("hub.db"));
    let engine = MultimodalHubStore::open(&dir.path("engine")).unwrap();
    assert_eq!(
        engine.stats().unwrap().total + 1,
        lite.stats().unwrap().total,
        "everything but the one over-long id"
    );
}
