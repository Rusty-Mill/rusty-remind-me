//! What is only true of this backend. The protocol itself is the route
//! suite's (`tests/hub_routes_test.rs`), which runs against this store too.

use super::*;
use crate::record;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A fresh data directory, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "remind_me_hub_multimodal_{label}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn memory(id: &str, updated: &str) -> Record {
    record::parse(&json!({
        "id": id,
        "content": format!("content of {id}"),
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": updated,
    }))
    .expect("a valid memory")
}

fn tombstone(id: &str, updated: &str, deleted: &str) -> Record {
    record::parse(&json!({
        "id": id,
        "content": "gone",
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": updated,
        "deleted_at": deleted,
    }))
    .expect("a valid tombstone")
}

fn link(memory_id: &str, entity_id: &str) -> Record {
    record::parse(&json!({
        "record_type": "memory_entity",
        "memory_id": memory_id,
        "entity_id": entity_id,
        "created_at": "2026-08-01T00:00:00Z",
    }))
    .expect("a valid link")
}

fn seqs(store: &MultimodalHubStore) -> Vec<(String, i64)> {
    store
        .pull_memories(&PullQuery {
            cursor: PullCursor::Seq(0),
            exclude_node: None,
            full: false,
            limit: 500,
        })
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["id"].as_str().unwrap().to_string(),
                m["hub_seq"].as_i64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn every_table_survives_a_reopen_and_hub_seq_carries_on_above_it() {
    let dir = TempDir::new("reopen");
    {
        let store = MultimodalHubStore::open(&dir.0).unwrap();
        store
            .apply_record(&memory("m1", "2026-08-02T00:00:00Z"), Some("n1"))
            .unwrap();
        store
            .apply_record(&memory("m2", "2026-08-02T00:00:00Z"), Some("n1"))
            .unwrap();
        store.apply_record(&link("m1", "e1"), Some("n1")).unwrap();
    }
    let store = MultimodalHubStore::open(&dir.0).unwrap();
    assert_eq!(seqs(&store), vec![("m1".into(), 1), ("m2".into(), 2)]);
    assert_eq!(store.stats().unwrap().memory_entities, 1);

    store
        .apply_record(&memory("m3", "2026-08-02T00:00:00Z"), None)
        .unwrap();
    assert_eq!(seqs(&store).last(), Some(&("m3".to_string(), 3)));
}

#[test]
fn compacting_away_the_highest_hub_seq_never_lets_it_be_issued_again() {
    // A node whose cursor sits at the deleted row's hub_seq would skip a
    // new row that reused it, for good.
    let dir = TempDir::new("seq_floor");
    {
        let store = MultimodalHubStore::open(&dir.0).unwrap();
        store
            .apply_record(&memory("m1", "2026-08-02T00:00:00Z"), None)
            .unwrap();
        store
            .apply_record(
                &tombstone("m2", "2026-08-02T00:00:00Z", "2026-08-02T00:00:00Z"),
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .compact_tombstones("2026-09-01T00:00:00+00:00")
                .unwrap(),
            1
        );
    }
    let store = MultimodalHubStore::open(&dir.0).unwrap();
    assert_eq!(seqs(&store), vec![("m1".into(), 1)]);
    store
        .apply_record(&memory("m3", "2026-08-03T00:00:00Z"), None)
        .unwrap();
    assert_eq!(seqs(&store).last(), Some(&("m3".to_string(), 3)));
}

#[test]
fn an_id_over_the_cap_is_a_storage_error_and_writes_nothing() {
    let dir = TempDir::new("id_cap");
    let store = MultimodalHubStore::open(&dir.0).unwrap();
    let long = "x".repeat(ID_CAP + 1);
    let err = store
        .apply_record(&memory(&long, "2026-08-02T00:00:00Z"), None)
        .unwrap_err();
    assert!(err.0.contains("at most 64 bytes"), "{}", err.0);
    assert!(store.apply_record(&link("m1", &long), None).is_err());
    assert_eq!(store.stats().unwrap().total, 0);

    // The id at the cap is fine, and does not burn a hub_seq on the way.
    store
        .apply_record(&memory(&"x".repeat(ID_CAP), "2026-08-02T00:00:00Z"), None)
        .unwrap();
    assert_eq!(seqs(&store)[0].1, 1);
}

#[test]
fn a_panic_under_the_lock_does_not_take_later_requests_down() {
    let dir = TempDir::new("poison");
    let store = MultimodalHubStore::open(&dir.0).unwrap();
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = store.write();
        panic!("a request panics while holding the lock");
    }));
    assert!(poisoned.is_err());
    assert!(store.tables.is_poisoned());

    assert!(store
        .apply_record(&memory("m1", "2026-08-02T00:00:00Z"), None)
        .unwrap());
    assert_eq!(store.stats().unwrap().total, 1);
}

#[test]
fn a_second_hub_cannot_open_a_directory_in_use() {
    let dir = TempDir::new("dir_lock");
    let first = MultimodalHubStore::open(&dir.0).unwrap();
    let err = MultimodalHubStore::open(&dir.0).err().expect("refused");
    assert!(err.0.contains("in use"), "{}", err.0);
    drop(first);
    assert!(MultimodalHubStore::open(&dir.0).is_ok());
}

#[test]
fn compaction_is_free_with_nothing_to_fold_and_loses_nothing_when_it_runs() {
    let dir = TempDir::new("compact");
    {
        let store = MultimodalHubStore::open(&dir.0).unwrap();
        assert!(!store.compact().unwrap(), "nothing written yet");
        store
            .apply_record(&memory("m1", "2026-08-02T00:00:00Z"), None)
            .unwrap();
        store
            .apply_record(&memory("m1", "2026-08-03T00:00:00Z"), None)
            .unwrap();
        assert!(store.compact().unwrap());
        assert!(!store.compact().unwrap(), "folded already");
        assert!(
            !dir.0.join("memories.mmap.inserts").exists(),
            "compaction removes the insert log"
        );
    }
    let store = MultimodalHubStore::open(&dir.0).unwrap();
    let pulled = store
        .pull_memories(&PullQuery {
            cursor: PullCursor::Seq(0),
            exclude_node: None,
            full: false,
            limit: 10,
        })
        .unwrap();
    assert_eq!(pulled.len(), 1);
    assert_eq!(pulled[0]["updated_at"], "2026-08-03T00:00:00+00:00");
    assert_eq!(pulled[0]["hub_seq"], 2);
}

#[test]
fn an_lww_win_keeps_the_original_created_at_as_the_sql_upsert_does() {
    let dir = TempDir::new("created_at");
    let store = MultimodalHubStore::open(&dir.0).unwrap();
    store
        .apply_record(&memory("m1", "2026-08-02T00:00:00Z"), None)
        .unwrap();
    let mut newer = json!({
        "id": "m1",
        "content": "edited",
        "created_at": "2026-08-09T00:00:00Z",
        "updated_at": "2026-08-10T00:00:00Z",
    });
    store
        .apply_record(&record::parse(&newer).unwrap(), None)
        .unwrap();
    let pulled = seqs(&store);
    assert_eq!(pulled, vec![("m1".into(), 2)]);
    let row = &store
        .pull_memories(&PullQuery {
            cursor: PullCursor::Seq(0),
            exclude_node: None,
            full: false,
            limit: 1,
        })
        .unwrap()[0];
    assert_eq!(row["content"], "edited");
    assert_eq!(row["created_at"], "2026-08-01T00:00:00+00:00");
    newer["updated_at"] = json!("2026-08-10T00:00:00Z");
    assert!(!store
        .apply_record(&record::parse(&newer).unwrap(), None)
        .unwrap());
}

#[test]
fn a_malformed_cursor_timestamp_is_an_error_not_an_empty_page() {
    let dir = TempDir::new("cursor");
    let store = MultimodalHubStore::open(&dir.0).unwrap();
    let err = store
        .pull_memories(&PullQuery {
            cursor: PullCursor::Since("yesterday".into()),
            exclude_node: None,
            full: false,
            limit: 10,
        })
        .unwrap_err();
    assert!(err.0.contains("yesterday"), "{}", err.0);
}
