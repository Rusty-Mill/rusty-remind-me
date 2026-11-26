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
    assert!(!dir.join("remind_me.db.dialogs").exists());
}

#[test]
fn on_appends_the_dialog_keyed_by_its_memory_id() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    test_env::set_var(ENV, "1");
    let (dir, db) = fresh("on");
    let made = auto_capture(&db.store(), &input()).unwrap();
    test_env::remove_var(ENV);
    let dialogs = SqliteDialogs::open(&dir.join("remind_me.db.dialogs")).unwrap();
    assert_eq!(dialogs.count().unwrap(), 1);
    let rows = dialogs.for_capture(&made.capture_id).unwrap();
    assert_eq!(rows[0].id, made.dialog_id);
}

#[test]
fn a_failing_store_leaves_the_capture_intact_and_is_counted() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    test_env::set_var(ENV, "1");
    let (dir, db) = fresh("fail");
    std::fs::write(dir.join("remind_me.db.dialogs"), b"in the way").unwrap();
    let before = failures();
    let made = auto_capture(&db.store(), &input()).unwrap();
    test_env::remove_var(ENV);
    assert_eq!(failures(), before + 1);
    assert!(!made.dialog_id.is_empty());
}

#[test]
fn two_databases_in_one_directory_keep_separate_shadow_stores() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    test_env::set_var(ENV, "1");
    let (dir, a) = fresh("two");
    let b = Database::open(dir.join("other.db")).unwrap();
    let made_a = auto_capture(&a.store(), &input()).unwrap();
    let made_b = auto_capture(&b.store(), &input()).unwrap();
    test_env::remove_var(ENV);
    let in_a = SqliteDialogs::open(&dir.join("remind_me.db.dialogs")).unwrap();
    let in_b = SqliteDialogs::open(&dir.join("other.db.dialogs")).unwrap();
    assert_eq!(in_a.for_capture(&made_a.capture_id).unwrap().len(), 1);
    assert!(in_a.for_capture(&made_b.capture_id).unwrap().is_empty());
    assert_eq!(in_b.for_capture(&made_b.capture_id).unwrap().len(), 1);
    assert!(in_b.for_capture(&made_a.capture_id).unwrap().is_empty());
}

fn transcript(session: &str, turns: &[&str]) -> String {
    turns
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            format!(
                "{}\n",
                serde_json::json!({"type": role, "sessionId": session,
                    "message": {"role": role, "content": text}})
            )
        })
        .collect()
}

#[test]
fn a_first_capture_and_its_continuation_land_together_redacted() {
    use remind_me_core::transcript::{capture_transcript, TranscriptRequest};
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    test_env::set_var(ENV, "1");
    let (dir, db) = fresh("cont");
    let secret = format!("ghp_{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8");
    let request = TranscriptRequest {
        session_id: "s1",
        cwd: None,
        reason: None,
        context: None,
    };
    let first = transcript("s1", &["hello", "hi there"]);
    capture_transcript(&db.store(), &first, &request).unwrap();
    let grown = transcript(
        "s1",
        &[
            "hello",
            "hi there",
            &format!("my token is {secret}"),
            "noted",
        ],
    );
    capture_transcript(&db.store(), &grown, &request).unwrap();
    test_env::remove_var(ENV);

    let dialogs = SqliteDialogs::open(&dir.join("remind_me.db.dialogs")).unwrap();
    assert_eq!(
        dialogs.count().unwrap(),
        2,
        "first row and one continuation"
    );
    let blobs =
        remind_me_core::blobs::BlobStore::new(dir.join("remind_me.db.dialogs").join("blobs"));
    let mut seen = String::new();
    for id in all_ids(&db) {
        let rec = dialogs.get(&id).unwrap().unwrap();
        seen.push_str(&String::from_utf8(blobs.get(&rec.body_hash).unwrap().unwrap()).unwrap());
    }
    assert!(
        !seen.contains(&secret),
        "the secret never reaches the shadow store"
    );
    assert!(seen.contains("REDACTED"));
}

/// Ids of the dialog rows the engine holds for session `s1`.
fn all_ids(db: &Database) -> Vec<String> {
    remind_me_core::episodes::session_memories(&db.store(), "s1")
        .unwrap()
        .into_iter()
        .filter(|m| m.category == remind_me_core::DIALOG_CATEGORY)
        .map(|m| m.id)
        .collect()
}

#[test]
fn a_summary_whose_category_is_dialog_is_not_shadowed_only_the_transcript() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    test_env::set_var(ENV, "1");
    let (dir, db) = fresh("role");
    let mut odd = input();
    odd.category = "dialog".into();
    let made = auto_capture(&db.store(), &odd).unwrap();
    test_env::remove_var(ENV);
    let dialogs = SqliteDialogs::open(&dir.join("remind_me.db.dialogs")).unwrap();
    let rows = dialogs.for_capture(&made.capture_id).unwrap();
    assert_eq!(rows.len(), 1, "the summary is not a transcript");
    assert_eq!(rows[0].id, made.dialog_id);
}

#[test]
fn a_committed_row_that_cannot_be_read_back_is_counted_not_dropped() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    test_env::set_var(ENV, "1");
    let (dir, db) = fresh("lookup");
    let before = failures();
    remind_me_core::dialog_shadow::record_committed(&db.store(), "mem_missing");
    test_env::remove_var(ENV);
    assert_eq!(failures(), before + 1);
    assert!(!dir.join("remind_me.db.dialogs").exists());
    // Off: no lookup, no count.
    let before = failures();
    remind_me_core::dialog_shadow::record_committed(&db.store(), "mem_missing");
    assert_eq!(failures(), before);
}
