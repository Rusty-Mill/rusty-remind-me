//! The dialog shadow write (#528, D82): off by default, on by flag, and a
//! failure never fails the capture.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::capture::auto_capture;
use remind_me_core::db::dialogs::DialogStore;
use remind_me_core::db::sqlite::SqliteDialogs;
use remind_me_core::dialog_shadow::{failures, ENV};
use remind_me_core::{AutoCaptureInput, Database};
use std::path::PathBuf;
use std::sync::Mutex;

/// The flag is process-global.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn input() -> AutoCaptureInput {
    AutoCaptureInput {
        conversation: "user: hi\nassistant: hello".into(),
        summary: "A greeting.".into(),
        title: String::new(),
        tags: vec![],
        category: "conversation".into(),
        metadata: serde_json::json!({}),
        project: None,
    }
}

fn fresh(name: &str) -> (PathBuf, Database) {
    let dir = std::env::temp_dir().join(format!("rrm_shadow_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = Database::open(dir.join("remind_me.db")).unwrap();
    (dir, db)
}

#[test]
fn off_by_default_touches_no_dialog_file() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    test_env::remove_var(ENV);
    let (dir, db) = fresh("off");
    auto_capture(&db.store(), &input()).unwrap();
    assert!(!dir.join("dialogs").exists());
}

#[test]
fn on_appends_the_dialog_keyed_by_its_memory_id() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    test_env::set_var(ENV, "1");
    let (dir, db) = fresh("on");
    let made = auto_capture(&db.store(), &input()).unwrap();
    test_env::remove_var(ENV);
    let dialogs = SqliteDialogs::open(&dir.join("dialogs")).unwrap();
    assert_eq!(dialogs.count().unwrap(), 1);
    let rows = dialogs.for_capture(&made.capture_id).unwrap();
    assert_eq!(rows[0].id, made.dialog_id);
}

#[test]
fn a_failing_store_leaves_the_capture_intact_and_is_counted() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    test_env::set_var(ENV, "1");
    let (dir, db) = fresh("fail");
    std::fs::write(dir.join("dialogs"), b"in the way").unwrap();
    let before = failures();
    let made = auto_capture(&db.store(), &input()).unwrap();
    test_env::remove_var(ENV);
    assert_eq!(failures(), before + 1);
    assert!(!made.dialog_id.is_empty());
}
