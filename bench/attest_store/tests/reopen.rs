//! Writes through a database on the Attest candidate survive a reopen,
//! which rebuilds the engine tables from the log alone.

use remind_me_core::db::queries;
use remind_me_core::models::MemoryAddInput;
use remind_me_core::{testing, Database};
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("attest_store_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn add(db: &Database, content: &str) -> String {
    let input = MemoryAddInput {
        content: content.into(),
        category: "fact".into(),
        ..Default::default()
    };
    queries::add_memory(&db.store(), input).unwrap().id
}

fn open(dir: &Path) -> Database {
    let tables = attest_store::open(&dir.join("log")).unwrap();
    Database::open_backend(Box::new(tables), Some(dir.join("memory.db"))).unwrap()
}

#[test]
fn writes_survive_a_reopen_from_the_log() {
    let dir = scratch("reopen");
    let (kept, gone) = {
        let db = open(&dir);
        let kept = add(&db, "quokkas are marsupials");
        let gone = add(&db, "deleted before the reopen");
        assert!(queries::delete_memory(&db.store(), &gone).unwrap());
        (kept, gone)
    };
    let db = open(&dir);
    let store = db.store();
    assert_eq!(
        testing::memory_text(&store, &kept, "content")
            .unwrap()
            .as_deref(),
        Some("quokkas are marsupials")
    );
    assert!(testing::memory_text(&store, &gone, "id").unwrap().is_none());
    let input = serde_json::from_value(serde_json::json!({ "query": "quokkas" })).unwrap();
    let found = queries::search_memories_with_embedder(&store, &input, None).unwrap();
    assert_eq!(
        found.len(),
        1,
        "the full-text index was rebuilt from the log"
    );
    drop(store);
    let again = add(&db, "written after the reopen");
    drop(db);
    let db = open(&dir);
    assert!(testing::memory_text(&db.store(), &again, "id")
        .unwrap()
        .is_some());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_second_opener_is_refused() {
    let dir = scratch("locked");
    let _first = open(&dir);
    assert!(attest_store::open(&dir.join("log")).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
