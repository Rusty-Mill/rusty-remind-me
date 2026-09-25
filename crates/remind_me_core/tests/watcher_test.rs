//! Coverage for the folder watcher.
//!
//! Every case drives [`Watcher::scan_once`] directly rather than waiting on the
//! interval — the debounce is about file *age*, not about how often scans run,
//! so a timing loop would only make the tests slow and flaky.

use remind_me_core::db::queries;
use remind_me_core::importer::connectors;
use remind_me_core::watcher::{
    disabled_status, supersede_import, validate_watch_dirs, ScanCounts, Watcher,
};
use remind_me_core::{Database, MemoryListInput};
use rusqlite::Connection;
use std::path::PathBuf;

/// A watch directory inside the default import root (the home directory).
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap()).join(format!(
        "rrm_watch_{}_{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A watcher with no grace period, so a freshly written file is ingested at
/// once. The debounce has its own tests.
fn watcher(dir: &std::path::Path) -> Watcher {
    Watcher::new(vec![dir.to_path_buf()], Vec::new()).with_grace(0)
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, body).unwrap();
    path
}

fn live_memories(conn: &Connection) -> Vec<String> {
    queries::list_memories(
        conn,
        &MemoryListInput {
            include_sensitive: false,
            limit: 100,
            ..Default::default()
        },
    )
    .unwrap()
    .memories
    .into_iter()
    .filter(|m| m.superseded_by.is_none())
    .map(|m| m.content)
    .collect()
}

// --- scanning ----------------------------------------------------------------

#[test]
fn a_new_file_is_ingested() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("new");
    write(&dir, "notes.md", "# Notes\n\nalpha");
    let mut w = watcher(&dir);

    let counts = w.scan_once(&conn);

    assert_eq!(counts.ingested, 1);
    assert_eq!(live_memories(&conn).len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_unchanged_file_is_not_work_on_the_next_pass() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("unchanged");
    write(&dir, "notes.md", "# Notes\n\nalpha");
    let mut w = watcher(&dir);
    w.scan_once(&conn);

    let second = w.scan_once(&conn);

    // Not ingested, not skipped, not counted at all — an unchanged signature
    // never reaches the importer.
    assert_eq!(second, ScanCounts::default());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_edited_file_is_re_ingested_and_supersedes_its_previous_import() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("edited");
    write(&dir, "notes.md", "# Notes\n\noriginal text");
    let mut w = watcher(&dir);
    w.scan_once(&conn);
    assert_eq!(
        live_memories(&conn),
        vec!["Notes\n\noriginal text".to_string()]
    );

    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(&dir, "notes.md", "# Notes\n\nrevised text");
    let counts = w.scan_once(&conn);

    assert_eq!(counts.ingested, 1);
    assert_eq!(counts.superseded, 1);
    // The old chunk stays in the database for audit but drops out of every
    // read path, so a stale version does not keep matching searches.
    assert_eq!(
        live_memories(&conn),
        vec!["Notes\n\nrevised text".to_string()]
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn supersession_leaves_a_deleted_memory_alone() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute(
        "INSERT INTO memories (id, content, category, tags, source, metadata,
                               created_at, updated_at, deleted_at)
         VALUES ('mem_gone', 'removed on purpose', 'general', '[]', 'document_import',
                 '{\"import_id\": \"imp_old\"}', '2026-01-01T00:00:00Z',
                 '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z')",
        [],
    )
    .unwrap();

    let superseded = supersede_import(&conn, "imp_old", "imp_new").unwrap();

    // Re-importing a changed file must not touch a memory the user explicitly
    // deleted — superseding it would be a silent write to a record they had
    // already decided about.
    assert_eq!(superseded, 0);
    let still_deleted: Option<String> = conn
        .query_row(
            "SELECT superseded_by FROM memories WHERE id = 'mem_gone'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(still_deleted.is_none());
}

#[test]
fn a_file_absent_for_n_scans_is_evicted_from_imports_and_no_longer_supersedes() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("evicted");
    let path = write(&dir, "notes.md", "# Notes\n\noriginal text");
    // A short threshold so the test does not need a thousand scans.
    let mut w = Watcher::new(vec![dir.clone()], Vec::new())
        .with_grace(0)
        .with_imports_stale_after_scans(3);
    w.scan_once(&conn);
    assert_eq!(
        live_memories(&conn),
        vec!["Notes\n\noriginal text".to_string()]
    );

    // Delete the file and scan past the eviction threshold.
    std::fs::remove_file(&path).unwrap();
    for _ in 0..3 {
        let counts = w.scan_once(&conn);
        assert_eq!(
            counts,
            ScanCounts::default(),
            "nothing to see, nothing to do"
        );
    }

    // The file returns with different content. Its `imports` entry is gone,
    // so this is a fresh import rather than a supersession: both versions
    // stay live rather than the old one being marked superseded.
    write(&dir, "notes.md", "# Notes\n\nrevised text after eviction");
    let counts = w.scan_once(&conn);

    assert_eq!(counts.ingested, 1);
    assert_eq!(
        counts.superseded, 0,
        "the evicted entry must not still supersede its old import"
    );
    let mut memories = live_memories(&conn);
    memories.sort();
    let mut expected = vec![
        "Notes\n\noriginal text".to_string(),
        "Notes\n\nrevised text after eviction".to_string(),
    ];
    expected.sort();
    assert_eq!(memories, expected);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_file_absent_for_fewer_than_n_scans_still_supersedes_on_return() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("not_evicted");
    let path = write(&dir, "notes.md", "# Notes\n\noriginal text");
    let mut w = Watcher::new(vec![dir.clone()], Vec::new())
        .with_grace(0)
        .with_imports_stale_after_scans(3);
    w.scan_once(&conn);

    // Absent for fewer scans than the threshold.
    std::fs::remove_file(&path).unwrap();
    w.scan_once(&conn);
    w.scan_once(&conn);

    // The file returns changed before eviction: it must still supersede.
    write(&dir, "notes.md", "# Notes\n\nrevised text");
    let counts = w.scan_once(&conn);

    assert_eq!(counts.ingested, 1);
    assert_eq!(
        counts.superseded, 1,
        "an entry that has not reached the absence threshold keeps its history"
    );
    assert_eq!(
        live_memories(&conn),
        vec!["Notes\n\nrevised text".to_string()]
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_file_watched_continuously_never_ages_out_of_imports() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("continuous");
    write(&dir, "notes.md", "# Notes\n\noriginal text");
    let mut w = Watcher::new(vec![dir.clone()], Vec::new())
        .with_grace(0)
        .with_imports_stale_after_scans(3);
    w.scan_once(&conn);

    // Many more scans than the eviction threshold, but the file is present
    // (and unchanged) every time, so its `imports` entry is never absent.
    for _ in 0..10 {
        w.scan_once(&conn);
    }

    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(&dir, "notes.md", "# Notes\n\nrevised after many scans");
    let counts = w.scan_once(&conn);

    assert_eq!(counts.ingested, 1);
    assert_eq!(
        counts.superseded, 1,
        "a continuously-present file must never age out of `imports`"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_file_already_imported_by_hand_is_skipped_and_adopted() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("adopted");
    let path = write(&dir, "notes.md", "# Notes\n\nalpha");
    // Someone imported it directly first — the restart case.
    remind_me_core::importer::import_chat(
        &conn,
        &remind_me_core::ChatImportInput {
            file_path: path.display().to_string(),
            category: "chat_import".into(),
            tags: vec![],
            extract_mode: "assistant_messages".into(),
            max_length: 10_000,
            kind: remind_me_core::ImportKind::Auto,
        },
    )
    .unwrap();
    let mut w = watcher(&dir);

    let counts = w.scan_once(&conn);
    assert_eq!(counts.skipped, 1);
    assert_eq!(counts.ingested, 0);

    // The import was adopted, so a later edit still supersedes it rather than
    // leaving both versions live.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(&dir, "notes.md", "# Notes\n\nrevised");
    let second = w.scan_once(&conn);
    assert_eq!(second.superseded, 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn subdirectories_are_scanned_but_hidden_ones_are_not() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("nested");
    write(&dir, "top.md", "# Top\n\nalpha");
    write(&dir.join("sub"), "deep.md", "# Deep\n\nbeta");
    write(&dir.join(".git"), "config.md", "# Config\n\ngamma");
    let mut w = watcher(&dir);

    let counts = w.scan_once(&conn);

    assert_eq!(
        counts.ingested, 2,
        "the two real files, not the one in .git"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_hidden_watch_directory_still_works() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("hidden")
        .parent()
        .unwrap()
        .join(format!(".rrm_watch_hidden_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write(&dir, "notes.md", "# Notes\n\nalpha");
    let mut w = watcher(&dir);

    // Hidden is judged relative to the watch root, so watching `~/.notes`
    // works while `.git` inside a watched folder is still skipped.
    assert_eq!(w.scan_once(&conn).ingested, 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unsupported_files_are_ignored() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("suffix");
    // Not `.png`: images became a supported format when OCR landed, so a
    // watcher now *does* pick one up. `.exe` is still nothing this reads.
    write(&dir, "installer.exe", "not markdown");
    let mut w = watcher(&dir);

    assert_eq!(w.scan_once(&conn), ScanCounts::default());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_missing_watch_directory_is_skipped_rather_than_failing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let absent = PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap())
        .join("rrm_watch_absent_99999");
    let mut w = Watcher::new(vec![absent], Vec::new()).with_grace(0);

    // It may be created later; a scan should not error on its absence.
    assert_eq!(w.scan_once(&conn), ScanCounts::default());
}

// --- debounce ----------------------------------------------------------------

#[test]
fn a_file_still_being_written_is_deferred() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("debounce");
    write(&dir, "notes.md", "# Notes\n\npartially written");
    // A long grace window makes the file unambiguously "too fresh".
    let mut w = Watcher::new(vec![dir.clone()], Vec::new()).with_grace(3_600);

    let first = w.scan_once(&conn);

    assert_eq!(first.debounced, 1);
    assert_eq!(first.ingested, 0);
    // Ingesting mid-write would store a truncated memory that dedup then pins
    // in place, because its hash is stable and wrong.
    assert!(live_memories(&conn).is_empty());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_deferred_file_is_ingested_once_its_signature_settles() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("settles");
    write(&dir, "notes.md", "# Notes\n\nfinished writing");
    let mut w = Watcher::new(vec![dir.clone()], Vec::new()).with_grace(3_600);
    assert_eq!(w.scan_once(&conn).debounced, 1);

    // A second scan sees the same (mtime, size): the file has stopped moving.
    let second = w.scan_once(&conn);

    assert_eq!(second.ingested, 1);
    assert_eq!(second.debounced, 0);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_file_that_keeps_changing_keeps_being_deferred() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("moving");
    write(&dir, "notes.md", "# Notes\n\nfirst");
    let mut w = Watcher::new(vec![dir.clone()], Vec::new()).with_grace(3_600);
    assert_eq!(w.scan_once(&conn).debounced, 1);

    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(&dir, "notes.md", "# Notes\n\nstill being written to");
    let second = w.scan_once(&conn);

    assert_eq!(second.debounced, 1, "a new signature restarts the wait");
    assert_eq!(second.ingested, 0);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_startup_backlog_ingests_immediately() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("backlog");
    let path = write(&dir, "notes.md", "# Notes\n\nwritten a while ago");
    // Backdate it well past the grace window — the ordinary case on restart.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3_600);
    filetime_set(&path, old);
    let mut w = Watcher::new(vec![dir.clone()], Vec::new()).with_grace(60);

    let counts = w.scan_once(&conn);

    // Only implementing the delay would give a watcher that waits before
    // touching anything at all on every restart.
    assert_eq!(counts.ingested, 1);
    assert_eq!(counts.debounced, 0);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// Set a file's modification time. `std::fs` cannot, and pulling in a crate for
/// one test would not be worth it.
fn filetime_set(path: &std::path::Path, when: std::time::SystemTime) {
    let secs = when
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::process::Command::new("touch")
        .arg("-d")
        .arg(format!("@{}", secs))
        .arg(path)
        .status()
        .expect("touch");
}

// --- containment -------------------------------------------------------------

#[test]
fn a_watch_dir_outside_the_import_roots_is_refused() {
    let (accepted, rejected) = validate_watch_dirs(&[PathBuf::from("/etc")]);

    // The watcher ingests through the same importer as the import tools, so it
    // inherits the same containment rule. A watch dir outside the roots would
    // be a way to import from anywhere by configuration.
    assert!(accepted.is_empty());
    assert_eq!(rejected.len(), 1);
    assert!(rejected[0].reason.contains("import roots"));
}

#[test]
fn a_watch_dir_inside_the_roots_is_accepted_even_if_it_does_not_exist_yet() {
    let home = PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap());
    let (accepted, rejected) = validate_watch_dirs(&[home.join("rrm_watch_future_dir_12345")]);

    assert_eq!(accepted.len(), 1);
    assert!(rejected.is_empty());
}

// --- status ------------------------------------------------------------------

#[test]
fn an_unconfigured_watcher_says_what_to_configure() {
    let status = disabled_status();

    assert!(!status.enabled);
    // A bare false could not tell "no watcher configured" apart from "the
    // watcher stopped".
    assert!(status.hint.is_some());
    assert!(status.hint.unwrap().contains("REMIND_ME_WATCH_DIRS"));
}

#[test]
fn the_status_reports_what_the_scans_did() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("status");
    write(&dir, "notes.md", "# Notes\n\nalpha");
    let mut w = watcher(&dir);
    w.scan_once(&conn);
    w.scan_once(&conn);

    let status = w.status();

    assert!(status.enabled);
    assert_eq!(status.scans, 2);
    assert_eq!(status.files_ingested, 1);
    assert!(status.last_scan_at.is_some());
    assert_eq!(status.watch_dirs.len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn refused_directories_are_reported_not_silently_dropped() {
    let dir = scratch("mixed");
    let (accepted, rejected) = validate_watch_dirs(&[dir.clone(), PathBuf::from("/etc")]);
    let w = Watcher::new(accepted, rejected);

    let status = w.status();

    assert_eq!(status.watch_dirs.len(), 1);
    assert_eq!(status.rejected_dirs.len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- connectors --------------------------------------------------------------

#[test]
fn the_connector_registry_lists_both_parsers() {
    let listed = connectors();

    let kinds: Vec<String> = listed.iter().map(|c| c.kind.clone()).collect();
    assert_eq!(
        kinds,
        vec![
            "chat".to_string(),
            "document".to_string(),
            "dbs".to_string(),
            "mempalace".to_string(),
        ]
    );
    assert!(listed.iter().all(|c| !c.description.is_empty()));
    // A document is prose, so it does not claim the chat-only formats.
    let document = listed.iter().find(|c| c.kind == "document").unwrap();
    assert!(!document.suffixes.contains(&"json".to_string()));
}

#[test]
fn a_connector_that_is_not_a_file_parser_says_so() {
    let listed = connectors();

    // Listed for discovery, not for dispatch: `remind_me_import_dbs` and
    // `remind_me_import_mempalace` read SQL rather than parsing a file, so
    // passing either as `kind` to `remind_me_import_chat` would go nowhere.
    // That is the question a caller reading this list is asking, so the flag
    // answers it.
    let bulk_importers = ["dbs", "mempalace"];
    for kind in bulk_importers {
        let connector = listed.iter().find(|c| c.kind == kind).unwrap();
        assert!(
            !connector.file_import_kind,
            "{kind} should not be dispatchable"
        );
        assert!(connector.suffixes.is_empty());
    }
    for parser in listed
        .iter()
        .filter(|c| !bulk_importers.contains(&c.kind.as_str()))
    {
        assert!(
            parser.file_import_kind,
            "{} should be dispatchable",
            parser.kind
        );
    }
}
