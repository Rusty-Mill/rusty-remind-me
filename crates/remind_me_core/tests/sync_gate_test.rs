//! A store's sync switch now that multi-node sync is gone (card #746).
//!
//! A store that synced keeps its bookkeeping across opens: its writes queue
//! outbox rows and its deletes leave tombstones, until `REMIND_ME_SYNC_DISABLE`
//! turns that off. Nothing turns it on, so a store that never synced stays
//! off whatever the environment says.
//!
//! Its own test binary, holding `ENV_LOCK` throughout: the switch reads
//! process-wide env vars, and a shared process would race the other files.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::queries;
use remind_me_core::models::MemoryAddInput;
use remind_me_core::sync::{store_syncs, SYNC_DISABLE_ENV};
use remind_me_core::testing::{self, Table};
use remind_me_core::Database;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Settings a syncing node carried before sync was removed.
const OLD_SYNC_SETTINGS: [(&str, &str); 3] = [
    ("REMIND_ME_NODE_ID", "node-gate-test"),
    ("REMIND_ME_HUB_URL", "http://hub.example"),
    ("REMIND_ME_SYNC_SECRET", "shh"),
];

/// A fresh directory for one store, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rrm_sync_gate_{tag}_{}",
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

fn add(path: &Path, content: &str) -> String {
    let db = Database::open(path).unwrap();
    let store = db.store();
    queries::add_memory(
        &store,
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

/// Open the store as a process with the current settings would, and read
/// the switch and the outbox.
fn reopen(path: &Path) -> (bool, i64) {
    let db = Database::open(path).unwrap();
    let store = db.store();
    (
        store_syncs(&store).unwrap(),
        testing::count(&store, Table::SyncOutbox).unwrap(),
    )
}

/// Turn `path` into a store that synced before sync was removed.
fn make_syncing(path: &Path) {
    let db = Database::open(path).unwrap();
    testing::mark_store_syncing(&db.store()).unwrap();
}

#[test]
fn a_syncing_store_keeps_syncing_across_opens() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    test_env::remove_var(SYNC_DISABLE_ENV);
    let scratch = Scratch::new("keep");
    make_syncing(&scratch.db());
    let kept = add(&scratch.db(), "stays queued");
    let (syncs, queued) = reopen(&scratch.db());
    assert!(syncs);
    assert!(queued > 0, "the add was queued");
    assert_eq!(
        reopen(&scratch.db()),
        (true, queued),
        "an open moves nothing"
    );

    // Its deletes still leave a tombstone, and its edits are still queued.
    let edited = add(&scratch.db(), "added later");
    {
        let db = Database::open(scratch.db()).unwrap();
        let store = db.store();
        assert!(queries::delete_memory(&store, &kept).unwrap());
        assert!(
            testing::memory_text(&store, &kept, "deleted_at")
                .unwrap()
                .is_some(),
            "the delete is a tombstone"
        );
        assert!(testing::memory_text(&store, &edited, "id")
            .unwrap()
            .is_some());
    }
    let (_, after_edits) = reopen(&scratch.db());
    assert!(after_edits > queued, "the later edits were queued");
}

#[test]
fn sync_disable_turns_sync_off_and_empties_the_outbox() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("disable");
    make_syncing(&scratch.db());
    add(&scratch.db(), "queued");

    test_env::set_var(SYNC_DISABLE_ENV, "yes");
    assert_eq!(reopen(&scratch.db()), (false, 0));

    // With the switch off, a delete is a hard delete, and it stays off.
    test_env::remove_var(SYNC_DISABLE_ENV);
    let gone = add(&scratch.db(), "to delete");
    {
        let db = Database::open(scratch.db()).unwrap();
        let store = db.store();
        assert!(queries::delete_memory(&store, &gone).unwrap());
        assert!(testing::memory_text(&store, &gone, "id").unwrap().is_none());
    }
    assert_eq!(reopen(&scratch.db()), (false, 0));
}

#[test]
fn old_sync_settings_never_turn_a_store_syncing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    test_env::remove_var(SYNC_DISABLE_ENV);
    for (key, value) in OLD_SYNC_SETTINGS {
        test_env::set_var(key, value);
    }
    let scratch = Scratch::new("never");
    let id = add(&scratch.db(), "local only");
    let db = Database::open(scratch.db()).unwrap();
    let store = db.store();
    assert!(!store_syncs(&store).unwrap());
    assert_eq!(testing::count(&store, Table::SyncOutbox).unwrap(), 0);
    assert!(queries::delete_memory(&store, &id).unwrap());
    assert!(testing::memory_text(&store, &id, "id").unwrap().is_none());
    for (key, _) in OLD_SYNC_SETTINGS {
        test_env::remove_var(key);
    }
}
