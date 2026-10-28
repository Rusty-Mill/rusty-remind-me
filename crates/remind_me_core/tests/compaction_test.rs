//! The store's periodic compaction (`Database::compact_store`) on an
//! on-disk store: it reclaims what writes left in the tables' logs,
//! compacts nothing the second time, and loses nothing.

use remind_me_core::db::queries;
use remind_me_core::models::MemoryAddInput;
use remind_me_core::testing;
use remind_me_core::Database;
use std::path::PathBuf;

/// A fresh directory for one store, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rrm_compaction_{tag}_{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("memory.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn add(db: &Database, content: &str) -> String {
    queries::add_memory(
        &db.store(),
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

#[test]
fn compaction_reclaims_the_logs_once_and_keeps_every_row() {
    let scratch = Scratch::new("once");
    let (kept, gone) = {
        let db = Database::open(scratch.db()).unwrap();
        let kept = add(&db, "kept across compaction");
        let gone = add(&db, "deleted before compaction");
        assert!(queries::delete_memory(&db.store(), &gone).unwrap());

        let compacted = db.compact_store().unwrap();
        assert!(compacted > 0, "the writes above left logs to fold");
        assert_eq!(
            db.compact_store().unwrap(),
            0,
            "nothing was written since, so nothing is rewritten"
        );
        let store = db.store();
        assert!(testing::memory_text(&store, &kept, "content")
            .unwrap()
            .is_some());
        (kept, gone)
    };

    // What compaction rewrote is what the next open reads.
    let db = Database::open(scratch.db()).unwrap();
    let store = db.store();
    assert_eq!(
        testing::memory_text(&store, &kept, "content")
            .unwrap()
            .as_deref(),
        Some("kept across compaction")
    );
    assert!(testing::memory_text(&store, &gone, "id").unwrap().is_none());
}
