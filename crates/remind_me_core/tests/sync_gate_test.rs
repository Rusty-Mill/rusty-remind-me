//! The store's sync switch across processes with different settings.
//!
//! A node's store is shared by every process on it, and they need not all
//! carry the sync settings: a real node ran its connector with them and its
//! dashboard without. Each open used to align the store with the opening
//! process, so the dashboard switched sync off, emptied the outbox and hard
//! deleted, and the connector switched it back on and queued everything
//! again. These tests reopen one store under changing settings, as those
//! processes did.
//!
//! Its own test binary, holding `ENV_LOCK` throughout: the switch is
//! process-wide env vars, and a shared process would race the other files.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::queries;
use remind_me_core::models::MemoryAddInput;
use remind_me_core::sync::{
    store_syncs, HUB_URL_ENV, NODE_ID_ENV, SYNC_DISABLE_ENV, SYNC_SECRET_ENV,
};
use remind_me_core::testing::{self, Table};
use remind_me_core::Database;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn configure_sync() {
    test_env::set_var(NODE_ID_ENV, "node-gate-test");
    test_env::set_var(HUB_URL_ENV, "http://hub.example");
    test_env::set_var(SYNC_SECRET_ENV, "shh");
    test_env::remove_var(SYNC_DISABLE_ENV);
}

fn unconfigure_sync() {
    test_env::remove_var(NODE_ID_ENV);
    test_env::remove_var(HUB_URL_ENV);
    test_env::remove_var(SYNC_SECRET_ENV);
    test_env::remove_var(SYNC_DISABLE_ENV);
}

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
            extract: true,
            attachments: vec![],
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

#[test]
fn a_process_without_sync_settings_leaves_a_syncing_store_syncing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("keep");
    configure_sync();
    let kept = add(&scratch.db(), "stays queued");
    let (syncs, queued) = reopen(&scratch.db());
    assert!(syncs);
    assert!(queued > 0, "the add was queued");

    // A dashboard started without the settings opens the same store.
    unconfigure_sync();
    assert_eq!(
        reopen(&scratch.db()),
        (true, queued),
        "neither the switch nor the outbox moves"
    );

    // Its edits are queued for the connector to push, and its deletes
    // leave a tombstone that can travel.
    let edited = add(&scratch.db(), "added by the dashboard");
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
    assert!(after_edits > queued, "the dashboard's edits were queued");

    // The connector opening again changes nothing: no re-queue of everything.
    configure_sync();
    assert_eq!(reopen(&scratch.db()), (true, after_edits));
    unconfigure_sync();
}

#[test]
fn sync_disable_turns_sync_off_and_empties_the_outbox() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("disable");
    configure_sync();
    add(&scratch.db(), "queued");

    unconfigure_sync();
    test_env::set_var(SYNC_DISABLE_ENV, "yes");
    assert_eq!(reopen(&scratch.db()), (false, 0));

    // With the switch off, a delete is a hard delete, as before.
    test_env::remove_var(SYNC_DISABLE_ENV);
    let gone = add(&scratch.db(), "to delete");
    {
        let db = Database::open(scratch.db()).unwrap();
        let store = db.store();
        assert!(queries::delete_memory(&store, &gone).unwrap());
        assert!(testing::memory_text(&store, &gone, "id").unwrap().is_none());
    }
    unconfigure_sync();
}

#[test]
fn sync_disable_wins_over_the_sync_settings() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("override");
    configure_sync();
    add(&scratch.db(), "queued");
    test_env::set_var(SYNC_DISABLE_ENV, "1");
    assert_eq!(reopen(&scratch.db()), (false, 0));

    // Turned back on, the outbox is filled again: what was written while
    // sync was off still reaches the hub.
    test_env::remove_var(SYNC_DISABLE_ENV);
    let (syncs, queued) = reopen(&scratch.db());
    assert!(syncs);
    assert!(queued > 0, "the backfill queued the store again");
    unconfigure_sync();
}

#[test]
fn a_store_that_never_synced_hard_deletes_without_settings() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = Scratch::new("never");
    unconfigure_sync();
    let id = add(&scratch.db(), "local only");
    let db = Database::open(scratch.db()).unwrap();
    let store = db.store();
    assert!(!store_syncs(&store).unwrap());
    assert!(queries::delete_memory(&store, &id).unwrap());
    assert!(testing::memory_text(&store, &id, "id").unwrap().is_none());
}
