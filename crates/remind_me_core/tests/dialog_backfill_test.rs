//! The dialog backfill dry run (#528, D144, D145): counts what would move,
//! writes nothing.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::blobs::{content_key, BlobStore};
use remind_me_core::capture::auto_capture;
use remind_me_core::db::dialogs::{dir_beside, DialogLog, DialogStore, NewDialog};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::sqlite::SqliteDialogs;
use remind_me_core::dialog_backfill::plan;
use remind_me_core::dialog_shadow::{record_memory, ENV};
use remind_me_core::{AutoCaptureInput, Database};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The shadow flag is process-global.
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
        "rrm_backfill_{name}_{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Database::open(dir.join("remind_me.db")).unwrap();
    (dir, db)
}

fn capture(db: &Database, text: &str, shadow: bool) -> String {
    if shadow {
        test_env::set_var(ENV, "1");
    }
    let made = auto_capture(&db.store(), &input(text));
    test_env::remove_var(ENV);
    made.unwrap().dialog_id
}

fn store_dir(dir: &Path) -> PathBuf {
    dir_beside(&dir.join("remind_me.db")).unwrap()
}

/// Every file under `root` with its bytes, sorted by path. SQLite's own
/// `-wal` and `-shm` side files are left out: opening the store read only
/// may create or fold them without changing a row.
fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.to_string_lossy();
            if path.is_dir() {
                stack.push(path);
            } else if !(name.ends_with("-wal") || name.ends_with("-shm")) {
                let bytes = std::fs::read(&path).unwrap();
                out.push((path, bytes));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn with_no_store_every_row_would_append_and_nothing_is_created() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("none");
    capture(&db, "alpha", false);
    capture(&db, "beta", false);

    let report = plan(&db.store()).unwrap();

    assert_eq!((report.live, report.would_append), (2, 2));
    assert!(report.would_append_bytes > 0);
    assert_eq!((report.store_rows, report.target_rows), (0, 2));
    assert_eq!(report.findings(), 0, "{report:?}");
    assert!(
        !store_dir(&dir).exists(),
        "the dry run never creates a store"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_partly_shadowed_store_is_counted_and_left_byte_identical() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("partial");
    capture(&db, "alpha", true);
    capture(&db, "beta", false);
    let before = snapshot(&store_dir(&dir));

    let report = plan(&db.store()).unwrap();

    assert_eq!(report.already_present, 1);
    assert_eq!(report.would_append, 1);
    assert_eq!((report.store_rows, report.target_rows), (1, 2));
    assert_eq!(report.findings(), 0, "{report:?}");
    assert_eq!(snapshot(&store_dir(&dir)), before, "nothing is written");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_target_digest_is_the_store_an_apply_would_reach() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("digest");
    capture(&db, "alpha", true);
    let beta = capture(&db, "beta", false);
    let planned = plan(&db.store()).unwrap();
    assert_ne!(planned.store_digest, planned.target_digest);

    // What an apply does for the one missing row.
    let row = Memories::new(&db.store()).get_live(&beta).unwrap().unwrap();
    test_env::set_var(ENV, "1");
    record_memory(&db.store(), &row);
    test_env::remove_var(ENV);

    let after = plan(&db.store()).unwrap();
    assert_eq!(after.would_append, 0);
    assert_eq!(after.store_rows, planned.target_rows);
    assert_eq!(after.store_digest, planned.target_digest);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn drift_orphans_and_bad_blobs_are_findings() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("problems");
    let missing = capture(&db, "alpha", true);
    let drifted = capture(&db, "beta", false);
    let store = store_dir(&dir);
    let mut log = DialogLog::new(
        SqliteDialogs::open(&store).unwrap(),
        BlobStore::new(store.join("blobs")),
    );
    for (id, body) in [(drifted.as_str(), "other text"), ("mem_orphan", "nobody's")] {
        log.record(&NewDialog {
            id,
            capture_id: "cap_x",
            session_id: None,
            project: None,
            created_at: "2026-11-27T00:00:00Z",
            body,
        })
        .unwrap();
    }
    let row = log.store().get(&missing).unwrap().unwrap();
    std::fs::remove_file(BlobStore::new(store.join("blobs")).path(&row.body_hash)).unwrap();

    let report = plan(&db.store()).unwrap();

    assert_eq!(report.drift, [drifted]);
    assert_eq!(report.no_memory, ["mem_orphan"]);
    assert_eq!(report.blob_missing, [missing]);
    assert_eq!(report.would_append, 0);
    assert_eq!(report.findings(), 3);
    let _ = std::fs::remove_dir_all(dir);
}

/// Five old dialog rows with no capture id: two from one session, one
/// from an imported document, one from an import recorded in metadata,
/// one with nothing to group by.
fn old_imports(db: &Database) -> Vec<String> {
    let now = chrono::Utc::now().to_rfc3339();
    let row = |id: &str| NewMemory {
        category: "dialog".into(),
        source: "import".into(),
        ..NewMemory::new(id, format!("user: {id}"), &now)
    };
    let rows = [
        NewMemory {
            session_id: Some("s1".into()),
            ..row("mem_s1a")
        },
        NewMemory {
            session_id: Some("s1".into()),
            ..row("mem_s1b")
        },
        NewMemory {
            doc_id: Some("doc1".into()),
            ..row("mem_doc")
        },
        NewMemory {
            metadata: serde_json::json!({ "import_id": "imp1" }),
            ..row("mem_imp")
        },
        row("mem_lone"),
    ];
    let db_store = db.store();
    let memories = Memories::new(&db_store);
    for r in &rows {
        memories.insert(r).unwrap();
    }
    rows.iter().map(|r| r.id.clone()).collect()
}

/// The generated id of a group key (D148).
fn generated(key: &str) -> String {
    format!("cap_gen_{}", &content_key(key.as_bytes())[..32])
}

#[test]
fn rows_with_no_capture_id_move_under_one_generated_id_per_conversation() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("nocap");
    old_imports(&db);

    let report = plan(&db.store()).unwrap();

    assert_eq!((report.live, report.would_append), (5, 5));
    assert_eq!((report.minted_rows, report.minted_capture_ids), (5, 4));
    let by_key: Vec<(&str, u64, u64)> = report
        .minted_by_key
        .iter()
        .map(|(k, c)| (k.as_str(), c.groups, c.rows))
        .collect();
    assert_eq!(
        by_key,
        [
            ("doc", 1, 1),
            ("import", 1, 1),
            ("row", 1, 1),
            ("session", 1, 2)
        ]
    );
    assert_eq!(report.findings(), 0, "{report:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_captured_row_takes_no_generated_id() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("captured");
    capture(&db, "alpha", false);

    let report = plan(&db.store()).unwrap();

    assert_eq!((report.would_append, report.minted_rows), (1, 0));
    assert!(report.minted_by_key.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn generated_ids_are_stable_and_bind_the_target_digest() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("minted_digest");
    let ids = old_imports(&db);
    let planned = plan(&db.store()).unwrap();

    // What an apply does: each row under its generated id.
    let keys = [
        "session:import:s1",
        "session:import:s1",
        "doc:doc1",
        "import:imp1",
        "row:mem_lone",
    ];
    let store = store_dir(&dir);
    let mut log = DialogLog::new(
        SqliteDialogs::open(&store).unwrap(),
        BlobStore::new(store.join("blobs")),
    );
    {
        // The database lock is held until the block ends; `plan` takes it.
        let db_store = db.store();
        let memories = Memories::new(&db_store);
        for (id, key) in ids.iter().zip(keys) {
            let m = memories.get_live(id).unwrap().unwrap();
            log.record(&NewDialog {
                id: &m.id,
                capture_id: &generated(key),
                session_id: m.session_id.as_deref(),
                project: m.project.as_deref(),
                created_at: &m.created_at,
                body: &m.content,
            })
            .unwrap();
        }
    }

    let after = plan(&db.store()).unwrap();
    assert_eq!((after.would_append, after.already_present), (0, 5));
    assert_eq!(after.findings(), 0, "{after:?}");
    assert_eq!(after.store_digest, planned.target_digest);
    let _ = std::fs::remove_dir_all(dir);
}

/// The PR's cost figure: a 25k-memory store, half its dialogs shadowed. Run
/// by hand with
/// `cargo test --release --test dialog_backfill_test -- --ignored --nocapture`.
#[test]
#[ignore]
fn dry_run_timing_on_a_large_store() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("timing");
    for i in 0..12_500 {
        capture(
            &db,
            &format!("topic {i} {}", "word ".repeat(80)),
            i % 2 == 0,
        );
    }
    // Linux: reset the peak resident size, so it measures the dry run only.
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let started = std::time::Instant::now();
    let report = plan(&db.store()).unwrap();
    let peak = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM"))
                .map(str::to_string)
        });
    println!(
        "dry run: {} live, {} would append, {} present in {:?}; {peak:?}",
        report.live,
        report.would_append,
        report.already_present,
        started.elapsed()
    );
    assert_eq!(report.findings(), 0);
    let _ = std::fs::remove_dir_all(dir);
}
