//! The dialog store check (#528, D132): read only, reports integrity and
//! drift against `memories`, and counts rows not yet migrated.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::blobs::BlobStore;
use remind_me_core::capture::auto_capture;
use remind_me_core::db::dialogs::{dir_beside, DialogLog, DialogStore, NewDialog};
use remind_me_core::db::sqlite::SqliteDialogs;
use remind_me_core::dialog_reconcile::{check, last_findings, runs, start_for, Depth};
use remind_me_core::dialog_shadow::ENV;
use remind_me_core::{AutoCaptureInput, Database};
use std::path::PathBuf;
use std::sync::Mutex;

/// The shadow flag and the counters are process-global.
static LOCK: Mutex<()> = Mutex::new(());

fn input(text: &str) -> AutoCaptureInput {
    AutoCaptureInput {
        conversation: format!("user: {text}\nassistant: noted"),
        summary: format!("About {text}."),
        title: String::new(),
        tags: vec![],
        category: "conversation".into(),
        metadata: serde_json::json!({}),
        project: None,
    }
}

fn fresh(name: &str) -> (PathBuf, Database) {
    let dir = std::env::temp_dir().join(format!(
        "rrm_reconcile_{name}_{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Database::open(dir.join("remind_me.db")).unwrap();
    (dir, db)
}

/// Capture `text` with the shadow write on: a dialog in both places. Its
/// dialog id.
fn shadowed(db: &Database, text: &str) -> String {
    test_env::set_var(ENV, "1");
    let made = auto_capture(&db.store(), &input(text));
    test_env::remove_var(ENV);
    made.unwrap().dialog_id
}

/// Capture `text` with the shadow write off: a dialog in `memories` only.
fn unshadowed(db: &Database, text: &str) -> String {
    test_env::remove_var(ENV);
    auto_capture(&db.store(), &input(text)).unwrap().dialog_id
}

fn store_dir(dir: &std::path::Path) -> PathBuf {
    dir_beside(&dir.join("remind_me.db")).unwrap()
}

fn append(dir: &std::path::Path, id: &str, body: &str) {
    let store = store_dir(dir);
    DialogLog::new(
        SqliteDialogs::open(&store).unwrap(),
        BlobStore::new(store.join("blobs")),
    )
    .record(&NewDialog {
        id,
        capture_id: "cap_x",
        session_id: None,
        project: None,
        created_at: "2026-11-27T00:00:00Z",
        body,
    })
    .unwrap();
}

/// The blob path of the store row `id`.
fn blob_of(dir: &std::path::Path, id: &str) -> PathBuf {
    let store = store_dir(dir);
    let row = SqliteDialogs::open(&store)
        .unwrap()
        .get(id)
        .unwrap()
        .unwrap();
    BlobStore::new(store.join("blobs")).path(&row.body_hash)
}

#[test]
fn no_dialog_store_means_no_report_and_no_thread() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("none");
    unshadowed(&db, "alpha");
    assert_eq!(check(&db.store(), Depth::Quick).unwrap(), None);
    assert!(start_for(&db.store()).is_none());
    assert!(!store_dir(&dir).exists(), "the check never creates a store");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_shadowed_store_is_clean_and_unshadowed_rows_are_only_counted() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("clean");
    shadowed(&db, "alpha");
    unshadowed(&db, "beta");
    for depth in [Depth::Quick, Depth::Full] {
        let report = check(&db.store(), depth).unwrap().unwrap();
        assert_eq!(report.rows, 1);
        assert_eq!(report.findings(), 0, "{report:?}");
        assert_eq!(report.not_migrated, 1);
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn each_kind_of_problem_is_listed_by_id() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("problems");
    let missing = shadowed(&db, "alpha");
    let wrong_size = shadowed(&db, "beta");
    let only_in_memories = unshadowed(&db, "gamma");
    // A store row under a memory's id but with other content: drift.
    append(&dir, &only_in_memories, "not what memories holds");
    // A store row no memory has.
    append(&dir, "mem_orphan", "nobody's");
    std::fs::remove_file(blob_of(&dir, &missing)).unwrap();
    std::fs::write(blob_of(&dir, &wrong_size), b"x").unwrap();

    let report = check(&db.store(), Depth::Quick).unwrap().unwrap();
    assert_eq!(report.rows, 4);
    assert_eq!(report.blob_missing, [missing]);
    assert_eq!(report.blob_mismatch, [wrong_size]);
    assert_eq!(report.no_memory, ["mem_orphan"]);
    assert_eq!(report.drift, [only_in_memories]);
    assert_eq!(report.not_migrated, 0);
    assert_eq!(report.findings(), 4);
    assert_eq!(last_findings(), 4);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn only_the_full_check_sees_a_same_size_corruption() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("full");
    let id = shadowed(&db, "alpha");
    let blob = blob_of(&dir, &id);
    let len = std::fs::metadata(&blob).unwrap().len() as usize;
    std::fs::write(&blob, vec![b'z'; len]).unwrap();
    assert_eq!(
        check(&db.store(), Depth::Quick)
            .unwrap()
            .unwrap()
            .findings(),
        0
    );
    let full = check(&db.store(), Depth::Full).unwrap().unwrap();
    assert_eq!(full.blob_mismatch, [id]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_check_changes_nothing() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("readonly");
    let id = shadowed(&db, "alpha");
    unshadowed(&db, "beta");
    std::fs::remove_file(blob_of(&dir, &id)).unwrap();
    let file = store_dir(&dir).join(remind_me_core::db::sqlite::DIALOGS_FILE);
    let before = std::fs::read(&file).unwrap();
    check(&db.store(), Depth::Full).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(
        SqliteDialogs::open(&store_dir(&dir))
            .unwrap()
            .count()
            .unwrap(),
        1
    );
    assert!(
        !blob_of(&dir, &id).exists(),
        "a missing blob is not restored"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_background_pass_runs_once_and_counts() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("thread");
    shadowed(&db, "alpha");
    let before = runs();
    start_for(&db.store()).unwrap().join().unwrap();
    assert_eq!(runs(), before + 1);
    assert_eq!(last_findings(), 0);
    let _ = std::fs::remove_dir_all(dir);
}

/// D132's bound: the quick pass over a 25k-memory store. Run by hand with
/// `cargo test --release --test dialog_reconcile_test -- --ignored --nocapture`.
#[test]
#[ignore]
fn quick_pass_timing_on_a_large_store() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("timing");
    test_env::set_var(ENV, "1");
    for i in 0..12_500 {
        auto_capture(
            &db.store(),
            &input(&format!("topic {i} {}", "word ".repeat(80))),
        )
        .unwrap();
    }
    test_env::remove_var(ENV);
    let started = std::time::Instant::now();
    let report = check(&db.store(), Depth::Quick).unwrap().unwrap();
    println!(
        "quick pass: {} rows in {:?}",
        report.rows,
        started.elapsed()
    );
    assert_eq!(report.findings(), 0);
    let _ = std::fs::remove_dir_all(dir);
}
