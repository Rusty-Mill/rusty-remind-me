//! Raw-transcript retention (#212).
//!
//! The property under test is the one the feature exists for: after an import,
//! a memory can hand back the *envelope* it came from — including the
//! `tool_use` and `thinking` blocks `text_of` deliberately dropped on the way
//! in. Asserting only that "an archive file appeared" would pass with the
//! spans wired to the wrong lines, which is the failure worth catching.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::archive::{
    self, ARCHIVE_DIR_ENV, ARCHIVE_MAX_AGE_DAYS_ENV, ARCHIVE_MAX_BYTES_ENV,
};
use remind_me_core::db::archives::Archives;
use remind_me_core::db::Store;
use remind_me_core::importer::import_chat;
use remind_me_core::testing;
use remind_me_core::undo_import::undo_import;
use remind_me_core::{
    ChatImportInput, Database, ImportKind, ImportOutcome, UndoImportInput, UndoImportKind,
};

/// `REMIND_ME_ARCHIVE_DIR` is process-global, so every test that sets it runs
/// serialized behind this. Poisoning is ignored: a panicking test has already
/// failed, and letting it block the rest converts one failure into all of them.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A scratch directory inside the default import root (the home directory),
/// matching `importer_test.rs`'s own helper — an import source has to sit
/// inside the import roots or containment refuses it.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap())
        .join(format!("rrm_archive_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One Claude Code transcript line, with machine chatter around the text.
fn transcript_line(text: &str, thinking: &str, tool_input: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "uuid": "11111111-2222-3333-4444-555555555555",
        "parentUuid": "00000000-0000-0000-0000-000000000000",
        "sessionId": "sess_abc",
        "timestamp": "2026-08-10T12:00:00Z",
        "cwd": "/home/user/project",
        "message": {
            "role": "assistant",
            "content": [
                { "type": "thinking", "thinking": thinking },
                { "type": "text", "text": text },
                { "type": "tool_use", "name": "Read", "input": { "file_path": tool_input } },
            ]
        }
    })
    .to_string()
}

fn import(store: &Store<'_>, path: &std::path::Path) -> ImportOutcome {
    import_chat(
        store,
        &ChatImportInput {
            file_path: path.display().to_string(),
            category: "chat_import".into(),
            tags: vec![],
            extract_mode: "all".into(),
            max_length: 10_000,
            kind: ImportKind::Auto,
        },
    )
    .unwrap()
}

/// Every memory id, in chunk order.
fn memory_ids(store: &Store<'_>) -> Vec<String> {
    let mut ids = testing::memory_ids(store).unwrap();
    ids.sort_by_key(|id| testing::memory_i64(store, id, "chunk_index").unwrap());
    ids
}

/// In memory, so `REMIND_ME_STORE=engine` runs these against the engine:
/// the archive blobs still go to a real directory.
fn open() -> Database {
    Database::open_in_memory().unwrap()
}

#[test]
fn retention_is_off_unless_the_directory_is_configured() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(ARCHIVE_DIR_ENV);

    assert!(archive::archive_root().is_none());
    assert!(!archive::is_enabled());
}

#[test]
fn an_import_with_retention_off_records_no_spans() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(ARCHIVE_DIR_ENV);

    let dir = scratch("off");
    let path = dir.join("chat.jsonl");
    std::fs::write(&path, transcript_line("a fact", "hmm", "/etc/hosts")).unwrap();

    let db = open();
    let store = db.store();
    import(&store, &path);

    // The import still happened...
    assert!(!memory_ids(&store).is_empty());
    // ...but nothing was retained, and the tables are present-and-empty rather
    // than missing: the read path must never have to tolerate absent tables.
    let spans = Archives::new(&store).span_count(None).unwrap();
    assert_eq!((spans, archive_rows(&store)), (0, 0));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_memory_can_recover_the_blocks_the_importer_dropped() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = scratch("roundtrip");
    let archive_dir = dir.join("archive");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, &archive_dir);

    let path = dir.join("chat.jsonl");
    std::fs::write(
        &path,
        transcript_line("the table is called memories", "let me check", "/etc/hosts"),
    )
    .unwrap();

    let db = open();
    let store = db.store();
    import(&store, &path);

    let ids = memory_ids(&store);
    assert_eq!(ids.len(), 1, "one text block, so one memory");

    // What the memory itself holds: the flattened text, nothing else.
    let stored: String = testing::memory_text(&store, &ids[0], "content")
        .unwrap()
        .unwrap();
    assert_eq!(stored, "the table is called memories");
    assert!(!stored.contains("let me check"));

    // What the archive hands back: the whole envelope, including everything
    // `text_of` dropped and every field the importer never read.
    let source = archive::source_for(&store, &ids[0], false)
        .unwrap()
        .unwrap();
    assert!(source.content.contains("let me check"), "thinking block");
    assert!(source.content.contains("tool_use"), "tool call");
    assert!(source.content.contains("sessionId"), "envelope metadata");
    assert!(source.content.contains("parentUuid"), "threading");
    assert!(!source.truncated);
    assert_eq!(source.filename, "chat.jsonl");

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn each_memory_points_at_its_own_line_not_the_whole_file() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = scratch("spans");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));

    let path = dir.join("chat.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n",
            transcript_line("first answer", "first thought", "/one"),
            transcript_line("second answer", "second thought", "/two"),
        ),
    )
    .unwrap();

    let db = open();
    let store = db.store();
    import(&store, &path);

    let ids = memory_ids(&store);
    assert_eq!(ids.len(), 2);

    let first = archive::source_for(&store, &ids[0], false)
        .unwrap()
        .unwrap();
    let second = archive::source_for(&store, &ids[1], false)
        .unwrap()
        .unwrap();

    // The whole point: a span is one envelope, not the file. Returning the
    // entire transcript for every memory would satisfy "content contains the
    // thinking block" while making drill-down worthless.
    assert!(first.content.contains("first thought"));
    assert!(!first.content.contains("second thought"));
    assert!(second.content.contains("second thought"));
    assert!(!second.content.contains("first thought"));
    assert_ne!(
        (first.byte_start, first.byte_end),
        (second.byte_start, second.byte_end)
    );

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn undoing_an_import_takes_its_archive_with_it() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = scratch("undo");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));

    let path = dir.join("chat.jsonl");
    std::fs::write(&path, transcript_line("a fact", "a thought", "/one")).unwrap();

    let db = open();
    let store = db.store();
    let outcome = import(&store, &path);
    let import_id = match outcome {
        ImportOutcome::Imported { import_id, .. } => import_id,
        other => panic!("expected an import, got {:?}", other),
    };

    let blob = archive_path(&store, &import_id);
    assert!(std::path::Path::new(&blob).exists());

    undo_import(
        &store,
        &UndoImportInput {
            import_kind: UndoImportKind::Chat,
            import_id: Some(import_id.clone()),
            dry_run: false,
            limit: 100,
        },
    )
    .unwrap();

    // `undo_import` drops the tracking row so the content is re-importable.
    // An archive left behind would be a blob nothing references, and its
    // spans would point at memories that no longer exist.
    let repo = Archives::new(&store);
    let archives = usize::from(repo.blob_of(&import_id).unwrap().is_some());
    let spans = repo.span_count(Some(&import_id)).unwrap();
    assert_eq!((archives, spans), (0, 0), "no orphaned archive rows");
    assert!(
        !std::path::Path::new(&blob).exists(),
        "the blob itself should be gone too"
    );

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Backdate an archive row so age-based retention has something to bite on.
/// Faster and more deterministic than waiting a day.
fn backdate(store: &Store<'_>, import_id: &str, days: i64) {
    let repo = Archives::new(store);
    let mut row = repo
        .oldest_first()
        .unwrap()
        .into_iter()
        .find(|r| r.import_id == import_id)
        .expect("an archive to backdate");
    row.archived_at = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    repo.record(&row).unwrap();
}

fn archive_rows(store: &Store<'_>) -> usize {
    Archives::new(store).oldest_first().unwrap().len()
}

fn archive_path(store: &Store<'_>, import_id: &str) -> String {
    Archives::new(store).blob_of(import_id).unwrap().unwrap().0
}

fn clear_limits() {
    crate::test_env::remove_var(ARCHIVE_MAX_AGE_DAYS_ENV);
    crate::test_env::remove_var(ARCHIVE_MAX_BYTES_ENV);
}

#[test]
fn with_no_limits_configured_pruning_removes_nothing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("nolimits");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));

    let path = dir.join("chat.jsonl");
    std::fs::write(&path, transcript_line("a fact", "t", "/one")).unwrap();
    let db = open();
    let store = db.store();
    let import_id = match import(&store, &path) {
        ImportOutcome::Imported { import_id, .. } => import_id,
        other => panic!("expected an import, got {:?}", other),
    };
    backdate(&store, &import_id, 4000);

    let report = archive::prune(&store, false).unwrap();

    // Someone already running with an archive chose to keep it. Turning on
    // silent deletion underneath them during an upgrade is the wrong way
    // round, so unset means unlimited — and the report says so, because zero
    // removals otherwise reads as "nothing was old enough".
    assert!(!report.limits_configured);
    assert_eq!(report.removed_for_age, 0);
    assert_eq!(archive_rows(&store), 1);

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_archive_past_the_age_limit_is_removed_but_its_memories_are_not() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("age");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));

    let path = dir.join("chat.jsonl");
    std::fs::write(&path, transcript_line("a fact", "a thought", "/one")).unwrap();
    let db = open();
    let store = db.store();
    let import_id = match import(&store, &path) {
        ImportOutcome::Imported { import_id, .. } => import_id,
        other => panic!("expected an import, got {:?}", other),
    };
    let ids = memory_ids(&store);
    let blob = archive_path(&store, &import_id);

    backdate(&store, &import_id, 40);
    crate::test_env::set_var(ARCHIVE_MAX_AGE_DAYS_ENV, "30");

    let report = archive::prune(&store, false).unwrap();

    assert!(report.limits_configured);
    assert_eq!(report.removed_for_age, 1);
    assert!(report.bytes_reclaimed > 0);
    assert_eq!(archive_rows(&store), 0);
    assert!(!std::path::Path::new(&blob).exists());

    // Pruning drops the archive, never the memories derived from it, and the
    // read path degrades to "no source" exactly as it does for an import made
    // before retention was switched on.
    assert_eq!(memory_ids(&store).len(), ids.len());
    assert!(archive::source_for(&store, &ids[0], false)
        .unwrap()
        .is_none());

    clear_limits();
    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dry_run_reports_without_removing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("dryrun");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));

    let path = dir.join("chat.jsonl");
    std::fs::write(&path, transcript_line("a fact", "t", "/one")).unwrap();
    let db = open();
    let store = db.store();
    let import_id = match import(&store, &path) {
        ImportOutcome::Imported { import_id, .. } => import_id,
        other => panic!("expected an import, got {:?}", other),
    };
    backdate(&store, &import_id, 40);
    crate::test_env::set_var(ARCHIVE_MAX_AGE_DAYS_ENV, "30");

    let report = archive::prune(&store, true).unwrap();

    assert!(report.dry_run);
    assert_eq!(report.removed_for_age, 1, "should say what it would remove");
    assert_eq!(archive_rows(&store), 1, "and then not remove it");

    clear_limits();
    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_size_ceiling_evicts_oldest_first_and_keeps_the_newest() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("size");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    let db = open();
    let store = db.store();

    // Three distinct imports, each a few hundred bytes.
    let mut import_ids = Vec::new();
    for n in 0..3 {
        let path = dir.join(format!("chat{}.jsonl", n));
        std::fs::write(&path, transcript_line(&format!("fact {}", n), "t", "/x")).unwrap();
        match import(&store, &path) {
            ImportOutcome::Imported { import_id, .. } => import_ids.push(import_id),
            other => panic!("expected an import, got {:?}", other),
        }
    }
    for (age, id) in [(30, 0), (20, 1), (10, 2)] {
        backdate(&store, &import_ids[id], age);
    }

    let lens: Vec<i64> = Archives::new(&store)
        .oldest_first()
        .unwrap()
        .iter()
        .map(|r| r.byte_len)
        .collect();
    let total: i64 = lens.iter().sum();
    let one_row: i64 = lens.iter().copied().max().unwrap();
    assert!(total > one_row, "fixture needs distinct blobs");

    // A ceiling that fits roughly one of the three.
    crate::test_env::set_var(ARCHIVE_MAX_BYTES_ENV, (one_row + 8).to_string());
    let report = archive::prune(&store, false).unwrap();

    assert_eq!(report.removed_for_size, 2);
    assert_eq!(report.removed_for_age, 0);
    assert!(report.bytes_remaining <= (one_row + 8) as u64);

    // The newest survives: it is the one most likely to be drilled into.
    let survivors: Vec<String> = Archives::new(&store)
        .oldest_first()
        .unwrap()
        .into_iter()
        .map(|r| r.import_id)
        .collect();
    assert_eq!(survivors, [import_ids[2].clone()]);

    clear_limits();
    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_malformed_line_does_not_shift_the_spans_after_it() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = scratch("malformed");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));

    // The importer skips a bad line rather than failing. Offsets are counted
    // over `split_inclusive('\n')`, so the skip must not desynchronise them —
    // a `lines()`-based counter would drop the separator and slide every
    // subsequent span one byte left per line.
    let path = dir.join("chat.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\nnot json at all\n{}\n",
            transcript_line("before the break", "t1", "/one"),
            transcript_line("after the break", "t2", "/two"),
        ),
    )
    .unwrap();

    let db = open();
    let store = db.store();
    import(&store, &path);

    let ids = memory_ids(&store);
    assert_eq!(ids.len(), 2, "the bad line is skipped, the rest survive");

    let second = archive::source_for(&store, &ids[1], false)
        .unwrap()
        .unwrap();
    assert!(second.content.contains("after the break"));
    assert!(!second.content.contains("not json at all"));
    assert!(!second.content.contains("before the break"));

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}
