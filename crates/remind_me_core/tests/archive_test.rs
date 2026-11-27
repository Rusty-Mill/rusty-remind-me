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
        "uuid": format!("u-{text}"),
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

/// In memory: the archive blobs still go to a real directory.
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

/// A session captured twice keeps one archive: the grown transcript, which
/// extends the first byte for byte, with the first copy's blob unlinked.
/// Each dialog row reads back exactly the lines it was made from.
#[test]
fn a_recaptured_session_keeps_only_its_latest_transcript() {
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("session");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    let db = open();
    let store = db.store();

    let first = transcript_line("first answer", "thinking one", "/one");
    let made = capture_transcript(&store, &first, &session_request()).unwrap();
    let import_id = archive::session_import_id("sess_abc");
    let first_blob = archive_path(&store, &import_id);

    let second = transcript_line("second", "thinking two", "/two");
    let grown = format!("{first}\n{second}");
    capture_transcript(&store, &grown, &session_request()).unwrap();

    assert_eq!(
        archive_rows(&store),
        2,
        "one archive for the session, one for the continuation row"
    );
    assert!(
        !std::path::Path::new(&first_blob).exists(),
        "old copy unlinked"
    );
    assert_eq!(archived(&store, &import_id), grown);
    assert_eq!(source(&store, &made.dialog_id), first);
    let continuation = continuation_ids(&store, "sess_abc");
    let later = source(&store, &continuation[0]);
    assert_eq!(later, second);
    assert_eq!(
        archived(&store, &archive::row_import_id(&continuation[0])),
        second
    );
    assert!(
        later.contains("thinking two"),
        "what the dialog drops is kept"
    );

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

fn session_request() -> remind_me_core::transcript::TranscriptRequest<'static> {
    remind_me_core::transcript::TranscriptRequest {
        session_id: "sess_abc",
        cwd: None,
        reason: Some("stop"),
        context: None,
    }
}

/// `n` numbered transcript lines from `from`, each its own message.
fn lines(range: std::ops::RangeInclusive<usize>) -> Vec<String> {
    range
        .map(|n| transcript_line(&format!("turn {n}"), &format!("thought {n}"), "/x"))
        .collect()
}

/// A line with its message id removed, so placement falls back to text.
fn without_id(line: &str) -> String {
    let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
    value.as_object_mut().unwrap().remove("uuid");
    value.to_string()
}

/// The raw source of `memory_id`, which must have one.
fn source(store: &Store<'_>, memory_id: &str) -> String {
    archive::source_for(store, memory_id, true)
        .unwrap()
        .unwrap_or_else(|| panic!("{memory_id} has a source"))
        .content
}

fn archived(store: &Store<'_>, import_id: &str) -> String {
    String::from_utf8(archive::archived_bytes(store, import_id).unwrap().unwrap()).unwrap()
}

/// The session's continuation rows, in sequence order.
fn continuation_ids(store: &Store<'_>, session_id: &str) -> Vec<String> {
    let dialog = remind_me_core::episodes::session_memories(store, session_id)
        .unwrap()
        .into_iter()
        .find(|m| m.metadata["type"] == "dialog")
        .unwrap();
    let capture =
        remind_me_core::capture::get_capture(store, dialog.capture_id.as_deref().unwrap())
            .unwrap()
            .unwrap();
    capture
        .continuations()
        .iter()
        .map(|m| m.id.clone())
        .collect()
}

/// The whole session, then only a later tail (sent over the size limit),
/// then that tail again. The tail does not extend the archived transcript,
/// so the continuation keeps its own lines under its row and the session's copy is left whole; the retry
/// adds and replaces nothing. Every row reads back its own lines.
#[test]
fn a_tail_never_replaces_the_whole_session_and_a_retry_changes_nothing() {
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("session-tail");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    let db = open();
    let store = db.store();
    let all = lines(1..=6);
    let session = archive::session_import_id("sess_abc");

    let full = all[..4].join("\n");
    let made = capture_transcript(&store, &full, &session_request()).unwrap();
    let tail = all[2..].join("\n");
    capture_transcript(&store, &tail, &session_request()).unwrap();

    let check = |label: &str| {
        assert_eq!(
            archived(&store, &session),
            full,
            "{label}: session copy whole"
        );
        assert_eq!(source(&store, &made.dialog_id), full, "{label}");
        let continuation = continuation_ids(&store, "sess_abc");
        assert_eq!(continuation.len(), 1, "{label}");
        assert_eq!(
            source(&store, &continuation[0]),
            all[4..].join("\n"),
            "{label}"
        );
        assert_eq!(
            archived(&store, &archive::row_import_id(&continuation[0])),
            all[4..].join("\n"),
            "{label}: only the row's own lines"
        );
        assert_eq!(archive_rows(&store), 2, "{label}");
    };
    check("after the tail");

    capture_transcript(&store, &tail, &session_request()).unwrap();
    check("after the retry");

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An older, shorter copy is held: nothing is written, and the session's
/// archive keeps the longer transcript.
#[test]
fn a_held_copy_never_replaces_the_session_archive() {
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("session-held");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    let db = open();
    let store = db.store();
    let all = lines(1..=4);
    let full = all.join("\n");
    let made = capture_transcript(&store, &full, &session_request()).unwrap();

    capture_transcript(&store, &all[..2].join("\n"), &session_request()).unwrap();

    assert_eq!(
        archived(&store, &archive::session_import_id("sess_abc")),
        full
    );
    assert_eq!(source(&store, &made.dialog_id), full);
    assert!(continuation_ids(&store, "sess_abc").is_empty());
    assert_eq!(archive_rows(&store), 1);

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A transcript that cannot be placed is kept whole in its own row, with
/// its own archive; the session's archive is left as it was.
#[test]
fn an_unplaced_copy_never_replaces_the_session_archive() {
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("session-unplaced");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    let db = open();
    let store = db.store();
    let bare = |texts: &[&str]| {
        texts
            .iter()
            .map(|t| without_id(&transcript_line(t, "t", "/x")))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let first = bare(&["a1", "b1"]);
    let made = capture_transcript(&store, &first, &session_request()).unwrap();
    let other = bare(&["x1", "y1"]);
    capture_transcript(&store, &other, &session_request()).unwrap();

    let session = archive::session_import_id("sess_abc");
    assert_eq!(archived(&store, &session), first);
    assert_eq!(source(&store, &made.dialog_id), first);
    let continuation = continuation_ids(&store, "sess_abc");
    assert_eq!(source(&store, &continuation[0]), other);
    assert_eq!(
        archived(&store, &archive::row_import_id(&continuation[0])),
        other
    );

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An archive that cannot be written fails no capture: every row is still
/// recorded, and reads back no source rather than an error.
#[test]
fn an_unavailable_archive_fails_no_capture() {
    use remind_me_core::transcript::{capture_transcript, captured_messages};
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("session-unavailable");
    let blocker = dir.join("not-a-dir");
    std::fs::write(&blocker, b"a file where the archive should go").unwrap();
    crate::test_env::set_var(ARCHIVE_DIR_ENV, blocker.join("archive"));
    let db = open();
    let store = db.store();
    let all = lines(1..=4);

    let made = capture_transcript(&store, &all[..2].join("\n"), &session_request()).unwrap();
    capture_transcript(&store, &all.join("\n"), &session_request()).unwrap();

    assert_eq!(captured_messages(&store, "sess_abc").unwrap(), Some(4));
    let continuation = continuation_ids(&store, "sess_abc");
    assert_eq!(continuation.len(), 1);
    for id in [&made.dialog_id, &continuation[0]] {
        assert!(
            archive::source_for(&store, id, true).unwrap().is_none(),
            "{id}"
        );
    }
    assert_eq!(archive_rows(&store), 0);

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A capture whose archive write failed gets its source when the same
/// transcript is sent again once the archive is back (#1621): the first
/// row's exact bytes are kept once, and no dialog row is added.
#[test]
fn a_resent_capture_repairs_a_first_row_source_lost_to_an_outage() {
    use remind_me_core::transcript::{capture_transcript, captured_messages};
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("repair-first");
    let blocker = dir.join("not-a-dir");
    std::fs::write(&blocker, b"a file where the archive should go").unwrap();
    crate::test_env::set_var(ARCHIVE_DIR_ENV, blocker.join("archive"));
    let db = open();
    let store = db.store();
    let full = lines(1..=3).join("\n");

    let made = capture_transcript(&store, &full, &session_request()).unwrap();
    assert!(archive::source_for(&store, &made.dialog_id, true)
        .unwrap()
        .is_none());

    std::fs::remove_file(&blocker).unwrap();
    let again = capture_transcript(&store, &full, &session_request()).unwrap();
    assert_eq!(again.dialog_id, made.dialog_id);
    assert_eq!(source(&store, &made.dialog_id), full);
    assert_eq!(
        archived(&store, &archive::session_import_id("sess_abc")),
        full
    );

    let check = |label: &str| {
        assert!(continuation_ids(&store, "sess_abc").is_empty(), "{label}");
        assert_eq!(captured_messages(&store, "sess_abc").unwrap(), Some(3));
        assert_eq!(archive_rows(&store), 1, "{label}: kept once");
    };
    check("after the repair");
    capture_transcript(&store, &full, &session_request()).unwrap();
    check("after a second resend");
    assert_eq!(source(&store, &made.dialog_id), full);

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same for a continuation row: the grown transcript is recorded while
/// the archive is down, then resent once it is back. The continuation gets
/// exactly its own lines; the first row keeps its source.
#[test]
fn a_resent_capture_repairs_a_continuation_source_lost_to_an_outage() {
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("repair-continuation");
    let root = dir.join("archive");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, &root);
    let db = open();
    let store = db.store();
    let all = lines(1..=4);
    let first = all[..2].join("\n");
    let grown = all.join("\n");
    capture_transcript(&store, &first, &session_request()).unwrap();

    let blocker = dir.join("not-a-dir");
    std::fs::write(&blocker, b"a file where the archive should go").unwrap();
    crate::test_env::set_var(ARCHIVE_DIR_ENV, blocker.join("archive"));
    capture_transcript(&store, &grown, &session_request()).unwrap();
    let continuation = continuation_ids(&store, "sess_abc");
    assert_eq!(continuation.len(), 1);
    assert!(archive::source_for(&store, &continuation[0], true)
        .unwrap()
        .is_none());

    crate::test_env::set_var(ARCHIVE_DIR_ENV, &root);
    capture_transcript(&store, &grown, &session_request()).unwrap();
    assert_sources(&store, "sess_abc", &first, &[all[2..].join("\n")]);
    assert_eq!(archive_rows(&store), 2, "the session and the row");

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A resend that does not hold a row's messages exactly leaves that row
/// without a source rather than pointing it at other bytes.
#[test]
fn a_shorter_resend_does_not_repair_a_row_it_does_not_hold() {
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("repair-shorter");
    let blocker = dir.join("not-a-dir");
    std::fs::write(&blocker, b"a file where the archive should go").unwrap();
    crate::test_env::set_var(ARCHIVE_DIR_ENV, blocker.join("archive"));
    let db = open();
    let store = db.store();
    let all = lines(1..=4);
    let made = capture_transcript(&store, &all.join("\n"), &session_request()).unwrap();

    std::fs::remove_file(&blocker).unwrap();
    capture_transcript(&store, &all[..2].join("\n"), &session_request()).unwrap();
    assert!(archive::source_for(&store, &made.dialog_id, true)
        .unwrap()
        .is_none());
    assert_eq!(archive_rows(&store), 0);

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A resend whose ids match a row but whose text does not is not the row's
/// source: matching ids alone never attach bytes to a locked dialog.
#[test]
fn a_resend_with_matching_ids_but_other_text_does_not_repair() {
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("repair-other-text");
    let blocker = dir.join("not-a-dir");
    std::fs::write(&blocker, b"a file where the archive should go").unwrap();
    crate::test_env::set_var(ARCHIVE_DIR_ENV, blocker.join("archive"));
    let db = open();
    let store = db.store();
    let all = lines(1..=3);
    let made = capture_transcript(&store, &all.join("\n"), &session_request()).unwrap();

    std::fs::remove_file(&blocker).unwrap();
    let mut changed: serde_json::Value = serde_json::from_str(&all[1]).unwrap();
    changed["message"]["content"][1]["text"] = serde_json::json!("not what was said");
    let resent = [all[0].clone(), changed.to_string(), all[2].clone()].join("\n");
    capture_transcript(&store, &resent, &session_request()).unwrap();
    assert!(archive::source_for(&store, &made.dialog_id, true)
        .unwrap()
        .is_none());
    assert_eq!(archive_rows(&store), 0);

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A continuation of interleaved turns lost to an outage is repaired from
/// the same resend: its messages are found by id, not as one run.
#[test]
fn a_resent_capture_repairs_an_interleaved_continuation() {
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("repair-interleaved");
    let root = dir.join("archive");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, &root);
    let db = open();
    let store = db.store();
    let all = lines(1..=5);
    capture_transcript(&store, &all[..3].join("\n"), &session_request()).unwrap();

    let blocker = dir.join("not-a-dir");
    std::fs::write(&blocker, b"a file where the archive should go").unwrap();
    crate::test_env::set_var(ARCHIVE_DIR_ENV, blocker.join("archive"));
    // Held 1, new 4, held 2, new 5, held 3.
    let mixed = [&all[0], &all[3], &all[1], &all[4], &all[2]].map(String::as_str);
    let mixed = mixed.join("\n");
    capture_transcript(&store, &mixed, &session_request()).unwrap();
    let continuation = continuation_ids(&store, "sess_abc");
    assert_eq!(continuation.len(), 1);
    assert!(archive::source_for(&store, &continuation[0], true)
        .unwrap()
        .is_none());

    crate::test_env::set_var(ARCHIVE_DIR_ENV, &root);
    capture_transcript(&store, &mixed, &session_request()).unwrap();
    assert_eq!(
        source(&store, &continuation[0]),
        format!("{}\n{}", all[3], all[4])
    );

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The archived transcript is scrubbed as the dialog is: the source of a
/// capture never returns a secret its dialog redacted.
#[test]
fn an_archived_session_keeps_no_secret_its_dialog_redacted() {
    use remind_me_core::transcript::{capture_transcript, TranscriptRequest};
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("session-secret");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    let db = open();
    let store = db.store();
    let request = TranscriptRequest {
        session_id: "sess_secret",
        cwd: None,
        reason: Some("stop"),
        context: None,
    };
    let token = format!("ghp_{}", "b".repeat(36));

    let made = capture_transcript(
        &store,
        &transcript_line(&format!("push with {token}"), "thinking", "/one"),
        &request,
    )
    .unwrap();

    let source = archive::source_for(&store, &made.dialog_id, false)
        .unwrap()
        .expect("the dialog has a source");
    assert!(!source.content.contains(&token), "{}", source.content);
    assert!(source.content.contains("[REDACTED:github_token]"));
    assert!(source.content.contains("thinking"), "structure is kept");

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

fn request_for(session_id: &str) -> remind_me_core::transcript::TranscriptRequest<'_> {
    remind_me_core::transcript::TranscriptRequest {
        session_id,
        cwd: None,
        reason: Some("stop"),
        context: None,
    }
}

/// `line` moved to session `session_id`.
fn in_session(line: &str, session_id: &str) -> String {
    let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
    value["sessionId"] = session_id.into();
    value.to_string()
}

/// Every dialog row of `session_id` with the source it must read back.
fn assert_sources(store: &Store<'_>, session_id: &str, first: &str, rows: &[String]) {
    let dialog = remind_me_core::episodes::session_memories(store, session_id)
        .unwrap()
        .into_iter()
        .find(|m| m.metadata["type"] == "dialog")
        .unwrap();
    assert_eq!(source(store, &dialog.id), first, "{session_id} first row");
    let continuation = continuation_ids(store, session_id);
    assert_eq!(continuation.len(), rows.len(), "{session_id}");
    for (id, want) in continuation.iter().zip(rows) {
        assert_eq!(&source(store, id), want, "{session_id} row {id}");
    }
}

/// Two sessions with the same turns keep apart: each row reads back its
/// own session's bytes after either one grows, whether by a copy that
/// extends its archive or by a tail kept under the row.
#[test]
fn two_sessions_keep_their_exact_source_bytes_after_either_grows() {
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("two-sessions");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    let db = open();
    let store = db.store();
    let of =
        |sid: &str| -> Vec<String> { lines(1..=4).iter().map(|l| in_session(l, sid)).collect() };
    let (a, b) = (of("sess_a"), of("sess_b"));

    capture_transcript(&store, &a[..2].join("\n"), &request_for("sess_a")).unwrap();
    capture_transcript(&store, &b[..2].join("\n"), &request_for("sess_b")).unwrap();

    // A grows by a copy that extends its archive.
    capture_transcript(&store, &a[..3].join("\n"), &request_for("sess_a")).unwrap();
    assert_sources(&store, "sess_a", &a[..2].join("\n"), &[a[2].clone()]);
    assert_sources(&store, "sess_b", &b[..2].join("\n"), &[]);

    // B grows by a tail that starts mid-session.
    capture_transcript(&store, &b[1..].join("\n"), &request_for("sess_b")).unwrap();
    assert_sources(&store, "sess_a", &a[..2].join("\n"), &[a[2].clone()]);
    assert_sources(&store, "sess_b", &b[..2].join("\n"), &[b[2..].join("\n")]);
    assert_eq!(
        archived(&store, &archive::session_import_id("sess_a")),
        a[..3].join("\n")
    );
    assert_eq!(
        archived(&store, &archive::session_import_id("sess_b")),
        b[..2].join("\n")
    );

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// New turns interleaved with held ones: the row's source is exactly the
/// new turns' lines, never the held lines between them.
#[test]
fn interleaved_held_and_new_turns_keep_only_the_new_lines() {
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("interleaved");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    let db = open();
    let store = db.store();
    let all = lines(1..=5);

    capture_transcript(&store, &all[..3].join("\n"), &session_request()).unwrap();
    // Held 1, new 4, held 2, new 5, held 3.
    let mixed = [&all[0], &all[3], &all[1], &all[4], &all[2]].map(String::as_str);
    capture_transcript(&store, &mixed.join("\n"), &session_request()).unwrap();

    let continuation = continuation_ids(&store, "sess_abc");
    assert_eq!(continuation.len(), 1);
    assert_eq!(
        source(&store, &continuation[0]),
        format!("{}\n{}", all[3], all[4])
    );
    // Not an extension of the archived copy: the session keeps the first.
    assert_eq!(
        archived(&store, &archive::session_import_id("sess_abc")),
        all[..3].join("\n")
    );

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A plain-text message line in `session_id`, with no message id.
fn turn(session_id: &str, role: &str, text: &str) -> String {
    serde_json::json!({
        "type": role, "sessionId": session_id,
        "timestamp": "2026-11-21T10:00:00.000Z",
        "message": {"role": role, "content": text}
    })
    .to_string()
}

/// A tail holding only assistant turns, then a capture that cannot be
/// placed, then the real later snapshot. The tail is split by the roles the
/// record holds and placed; the ambiguous copy is kept flagged outside the
/// placed sequence; the snapshot is placed after the placed rows. Rows read
/// back in sequence and every row keeps its own source.
#[test]
fn a_partial_role_tail_then_an_ambiguous_copy_then_a_snapshot_stay_in_order() {
    use remind_me_core::transcript::{capture_transcript, captured_messages};
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("partial-ambiguous-snapshot");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    let db = open();
    let store = db.store();
    let sid = "sess_mix";
    let u = |t: &str| turn(sid, "user", t);
    let a = |t: &str| turn(sid, "assistant", t);
    let join = |ls: &[String]| ls.join("\n");

    let first = [u("q1"), a("ok"), u("q2"), a("r2")];
    capture_transcript(&store, &join(&first), &request_for(sid)).unwrap();
    // Assistant turns only: "r2" ends the record once, so "ok" is new.
    let tail = [a("r2"), a("ok")];
    capture_transcript(&store, &join(&tail), &request_for(sid)).unwrap();
    // "ok" now ends the record and occurs in it twice: ambiguous.
    let stray = [a("ok"), u("q9")];
    capture_transcript(&store, &join(&stray), &request_for(sid)).unwrap();
    // The real session so far.
    let snapshot = [
        u("q1"),
        a("ok"),
        u("q2"),
        a("r2"),
        a("ok"),
        u("q3"),
        a("r3"),
    ];
    capture_transcript(&store, &join(&snapshot), &request_for(sid)).unwrap();

    let dialog = remind_me_core::episodes::session_memories(&store, sid)
        .unwrap()
        .into_iter()
        .find(|m| m.metadata["type"] == "dialog")
        .unwrap();
    let capture =
        remind_me_core::capture::get_capture(&store, dialog.capture_id.as_deref().unwrap())
            .unwrap()
            .unwrap();
    let rows = capture.continuations();
    let seen: Vec<(u64, &str, u64)> = rows
        .iter()
        .map(|m| {
            (
                m.metadata["sequence"].as_u64().unwrap(),
                m.metadata["placement"].as_str().unwrap(),
                m.metadata["from_message"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (1, "overlap", 4),
            (2, "unplaced_ambiguous", 5),
            (3, "overlap", 5)
        ]
    );
    assert!(rows[2].content.contains("q3") && rows[2].content.contains("r3"));
    assert!(!rows[2].content.contains("q9"));
    assert_eq!(captured_messages(&store, sid).unwrap(), Some(9));

    // Provenance: the first row its transcript's lines, each continuation
    // only its own; the session archive was replaced by the snapshot alone,
    // which extends the first transcript.
    assert_sources(
        &store,
        sid,
        &join(&first),
        &[a("ok"), join(&stray), join(&snapshot[5..])],
    );
    assert_eq!(
        archived(&store, &archive::session_import_id(sid)),
        join(&snapshot)
    );

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `line` with message id `id`.
fn with_id(line: &str, id: &str) -> String {
    let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
    value["uuid"] = id.into();
    value.to_string()
}

/// How many messages the session's placed rows hold, as the watcher sees it.
fn placed(store: &Store<'_>, raw: &str, session_id: &str) -> usize {
    use remind_me_core::transcript::{capture_unless_held, Conditional, Replace};
    match capture_unless_held(store, raw, &request_for(session_id), Replace::Never).unwrap() {
        Conditional::Held(n) => n,
        other => panic!("{other:?}"),
    }
}

/// A legacy first row (no roles listed), an unplaced tail with ids, its
/// exact retry, then a tail with the same text and other ids. The retry
/// keeps one copy; the other ids are new turns, kept with their own exact
/// source; neither moves the placed watermark.
#[test]
fn an_equal_text_tail_with_other_ids_is_kept_and_an_exact_retry_is_not() {
    use remind_me_core::db::memories::{Memories, MemoryEdit};
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();

    let dir = scratch("unplaced-ids");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    let db = open();
    let store = db.store();
    let sid = "sess_leg";
    let first = [turn(sid, "user", "a1"), turn(sid, "assistant", "b1")].join("\n");
    let made = capture_transcript(&store, &first, &request_for(sid)).unwrap();
    let row = remind_me_core::db::queries::get_memory_by_id(&store, &made.dialog_id)
        .unwrap()
        .unwrap();
    let mut metadata = row.metadata.clone();
    metadata.as_object_mut().unwrap().remove("roles");
    Memories::new(&store)
        .apply_edit(
            &made.dialog_id,
            &MemoryEdit {
                metadata: Some(metadata),
                ..MemoryEdit::at("2026-11-21T10:00:01Z")
            },
        )
        .unwrap();
    assert_eq!(placed(&store, &first, sid), 2);

    let tail = |ids: [&str; 2]| {
        [
            with_id(&turn(sid, "assistant", "x1"), ids[0]),
            with_id(&turn(sid, "assistant", "y1"), ids[1]),
        ]
        .join("\n")
    };
    let kept = tail(["t1", "t2"]);
    capture_transcript(&store, &kept, &request_for(sid)).unwrap();
    capture_transcript(&store, &kept, &request_for(sid)).unwrap();
    assert_eq!(
        continuation_ids(&store, sid).len(),
        1,
        "the retry is one copy"
    );

    let other = tail(["t3", "t4"]);
    capture_transcript(&store, &other, &request_for(sid)).unwrap();

    let rows = continuation_ids(&store, sid);
    assert_eq!(rows.len(), 2, "other ids are new turns");
    assert_eq!(source(&store, &rows[0]), kept);
    assert_eq!(source(&store, &rows[1]), other);
    for id in &rows {
        let row = remind_me_core::db::queries::get_memory_by_id(&store, id)
            .unwrap()
            .unwrap();
        assert_eq!(row.metadata["placement"], "unplaced_no_overlap");
        assert_eq!(row.metadata["from_message"], 2);
    }
    assert_eq!(placed(&store, &first, sid), 2, "placed watermark unchanged");

    // Partially identified tails: one turn with an id, one without.
    let partial = |id: &str| {
        [
            with_id(&turn(sid, "assistant", "x2"), id),
            turn(sid, "assistant", "y2"),
        ]
        .join("\n")
    };
    let kept = partial("p1");
    capture_transcript(&store, &kept, &request_for(sid)).unwrap();
    capture_transcript(&store, &kept, &request_for(sid)).unwrap();
    assert_eq!(
        continuation_ids(&store, sid).len(),
        3,
        "the retry is one copy"
    );
    // Equal text, but the identified turn's known id differs.
    let other = partial("p2");
    capture_transcript(&store, &other, &request_for(sid)).unwrap();

    let rows = continuation_ids(&store, sid);
    assert_eq!(rows.len(), 4, "a distinct known id is a new turn");
    assert_eq!(source(&store, &rows[2]), kept);
    assert_eq!(source(&store, &rows[3]), other);
    let ids = |id: &str| {
        remind_me_core::db::queries::get_memory_by_id(&store, id)
            .unwrap()
            .unwrap()
            .metadata["message_ids"]
            .clone()
    };
    assert_eq!(ids(&rows[2]), serde_json::json!(["p1", null]));
    assert_eq!(ids(&rows[3]), serde_json::json!(["p2", null]));
    assert_eq!(placed(&store, &first, sid), 2, "placed watermark unchanged");

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// One flat `{role, content}` line, as the Hermes plugin sends a remote
/// capture through `remind_me_capture_transcript` (D133 key, capture token).
fn flat_line(role: &str, content: &str) -> String {
    serde_json::json!({ "role": role, "content": content }).to_string()
}

/// A Hermes remote capture: a retried callback and a grown transcript keep
/// one capture, dialog and summary; another client with the same Hermes
/// session id keeps its own; the archive is scrubbed.
#[test]
fn a_flat_hermes_capture_is_one_per_client_session_and_its_archive_is_scrubbed() {
    use remind_me_core::transcript::capture_transcript;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_limits();
    let dir = scratch("hermes-flat");
    crate::test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    let db = open();
    let store = db.store();
    let token = format!("ghp_{}", "c".repeat(36));
    let first = format!(
        "{}\n{}\n",
        flat_line("user", &format!("deploy with {token}")),
        flat_line("assistant", "deployed")
    );

    let made = capture_transcript(&store, &first, &request_for("hermes-jarvis:s1")).unwrap();
    let retried = capture_transcript(&store, &first, &request_for("hermes-jarvis:s1")).unwrap();
    assert_eq!(retried.capture_id, made.capture_id, "a retry adds nothing");
    assert_eq!(retried.summary_id, made.summary_id);

    let grown = format!("{first}{}\n", flat_line("user", "and roll back"));
    let again = capture_transcript(&store, &grown, &request_for("hermes-jarvis:s1")).unwrap();
    assert_eq!(again.capture_id, made.capture_id, "grown, not added");
    assert_eq!(
        again.summary_id, made.summary_id,
        "the summary stays linked"
    );
    assert_eq!(again.messages, 3);

    let other = capture_transcript(&store, &first, &request_for("hermes-gw:s1")).unwrap();
    assert_ne!(other.capture_id, made.capture_id, "clients never share");
    let gw = remind_me_core::episodes::session_memories(&store, "hermes-gw:s1").unwrap();
    assert!(gw.iter().all(|m| !m.content.contains("roll back")));

    let kept = archived(&store, &archive::session_import_id("hermes-jarvis:s1"));
    assert!(!kept.contains(&token), "{kept}");
    assert!(kept.contains("[REDACTED:github_token]"), "{kept}");

    crate::test_env::remove_var(ARCHIVE_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}
