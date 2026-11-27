//! The dialog rebuild check (#528, D133): replays dialogs from their
//! archived sources into a scratch store and compares, read only.
//!
//! Unix only: elsewhere the owner-only scratch is refused (a unit test in
//! `dialog_rebuild` covers that).
#![cfg(unix)]

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::archive::ARCHIVE_DIR_ENV;
use remind_me_core::capture::auto_capture;
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::dialog_rebuild::check;
use remind_me_core::dialog_shadow::ENV as SHADOW_ENV;
use remind_me_core::importer::TOOL_INPUT_CHARS;
use remind_me_core::skeleton::write_skeleton;
use remind_me_core::transcript::{
    capture_transcript, TranscriptRequest, MAX_DIALOG_CHARS, MESSAGE_TOOLS_ENV,
};
use remind_me_core::{AutoCaptureInput, Database, SkeletonWriteInput};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The archive, shadow and message-tools settings are process-global.
static LOCK: Mutex<()> = Mutex::new(());

/// A Luhn-valid card number: redaction replaces it.
const CARD: &str = "4111 1111 1111 1111";

fn fresh(name: &str) -> (PathBuf, Database) {
    let dir = std::env::temp_dir().join(format!(
        "rrm_rebuild_{name}_{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    test_env::set_var(ARCHIVE_DIR_ENV, dir.join("archive"));
    test_env::remove_var(MESSAGE_TOOLS_ENV);
    let db = Database::open(dir.join("remind_me.db")).unwrap();
    (dir, db)
}

fn done(dir: &Path) {
    test_env::remove_var(ARCHIVE_DIR_ENV);
    test_env::remove_var(SHADOW_ENV);
    test_env::remove_var(MESSAGE_TOOLS_ENV);
    let _ = std::fs::remove_dir_all(dir);
}

fn line(n: usize, role: &str, content: serde_json::Value) -> String {
    serde_json::json!({
        "type": role,
        "uuid": format!("u-{n}"),
        "sessionId": "sess_rebuild",
        "message": { "role": role, "content": content },
    })
    .to_string()
}

fn text(n: usize, role: &str, body: &str) -> String {
    line(n, role, serde_json::json!(body))
}

/// An assistant turn that replies through the claude.ai thread tool, which
/// a capture renders in full only while that tool is a message tool.
fn posted(n: usize, body: &str) -> String {
    line(
        n,
        "assistant",
        serde_json::json!([{
            "type": "tool_use",
            "name": "mcp__hearthbot__post_message",
            "input": { "text": body },
        }]),
    )
}

/// An assistant turn that runs `command` through Bash: a capture keeps a
/// preview of its first `TOOL_INPUT_CHARS`.
fn bash(n: usize, command: &str) -> String {
    line(
        n,
        "assistant",
        serde_json::json!([{
            "type": "tool_use",
            "name": "Bash",
            "input": { "command": command },
        }]),
    )
}

fn capture(db: &Database, lines: &[String]) -> String {
    capture_as(db, "sess_rebuild", lines)
}

fn capture_as(db: &Database, session_id: &str, lines: &[String]) -> String {
    let raw = lines.join("\n") + "\n";
    let request = TranscriptRequest {
        session_id,
        cwd: None,
        reason: Some("stop"),
        context: None,
    };
    capture_transcript(&db.store(), &raw, &request)
        .unwrap()
        .capture_id
}

/// A capture with no archived source; its capture id.
fn plain(db: &Database) -> String {
    let input = AutoCaptureInput {
        conversation: "user: hi\nassistant: hello".into(),
        summary: "A greeting.".into(),
        title: String::new(),
        tags: vec![],
        category: "conversation".into(),
        metadata: serde_json::json!({}),
        project: None,
    };
    auto_capture(&db.store(), &input).unwrap().capture_id
}

/// A one-node skeleton over `lines` of `capture_id`'s dialog.
fn sketch(db: &Database, capture_id: &str, lines: (usize, usize)) {
    let mut nodes = std::collections::BTreeMap::new();
    nodes.insert("n".to_string(), lines);
    let skeleton = SkeletonWriteInput {
        capture_id: capture_id.to_string(),
        mermaid: "graph TD; n".into(),
        nodes,
    };
    write_skeleton(&db.store(), &skeleton).unwrap();
}

#[test]
fn every_archived_dialog_replays_to_its_stored_body() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("clean");
    test_env::set_var(SHADOW_ENV, "1");
    let first = [
        text(1, "user", &format!("my card is {CARD}")),
        text(2, "assistant", "noted"),
    ];
    let capture_id = capture(&db, &first);
    let mut grown = first.to_vec();
    grown.push(text(3, "user", "and one more thing"));
    capture(&db, &grown); // a continuation row
    let unsourced = plain(&db); // no source: not replayable
    sketch(&db, &capture_id, (1, 1));
    sketch(&db, &unsourced, (1, 1));
    // A fact decomposed from the capture: linked by `source_capture_id`.
    let now = chrono::Utc::now().to_rfc3339();
    let fact = NewMemory {
        source_capture_id: Some(capture_id.clone()),
        ..NewMemory::new("mem_fact", "a decomposed fact", &now)
    };
    Memories::new(&db.store()).insert(&fact).unwrap();

    let live = Memories::new(&db.store()).all_live().unwrap();
    assert!(
        live.iter().all(|m| !m.content.contains(CARD)),
        "the card was redacted"
    );
    let scratch = dir.join("scratch");
    let report = check(&db.store(), Some(&scratch), false).unwrap();

    assert_eq!(report.findings(), 0, "{report:?}");
    assert_eq!((report.dialogs, report.replayed), (3, 2));
    assert_eq!(report.not_replayable, 1);
    assert_eq!(report.not_in_store, 0);
    // One skeleton checked on replayed lines, one only on live lines.
    assert_eq!(
        (report.skeletons_replayed, report.skeletons_live_only),
        (1, 1)
    );
    assert_eq!(report.source_capture_links_unchecked, 1);
    // Both sides by kind: every live memory, and only the replayed rows.
    let live_total = Memories::new(&db.store()).all_live().unwrap().len() as u64;
    let sides = report.by_kind.values();
    assert_eq!(sides.clone().map(|k| k.live).sum::<u64>(), live_total);
    assert_eq!(sides.map(|k| k.rebuilt).sum::<u64>(), report.replayed);
    assert!(report.redact);
    assert!(!report.kept && !scratch.exists());
    done(&dir);
}

#[test]
fn a_truncated_transcript_replays_with_its_cut() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("cut");
    let long = "y".repeat(MAX_DIALOG_CHARS + 50);
    capture(
        &db,
        &[text(1, "user", "start"), text(2, "assistant", &long)],
    );

    let report = check(&db.store(), Some(&dir.join("scratch")), true).unwrap();

    assert_eq!(report.findings(), 0, "{report:?}");
    assert_eq!(report.replayed, 1);
    // Kept on request; no dialog store, so the row is counted as missing.
    assert!(report.kept && dir.join("scratch").exists());
    assert_eq!(report.not_in_store, 1);
    done(&dir);
}

#[test]
fn a_cut_row_whose_source_was_redacted_is_counted_not_replayed() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("cut_redacted");
    // The card falls in the kept tail; its replacement has another length,
    // so the archive (redacted before the cut) would cut elsewhere.
    let long = format!("{} card {CARD}", "y".repeat(MAX_DIALOG_CHARS + 50));
    capture(
        &db,
        &[text(1, "user", "start"), text(2, "assistant", &long)],
    );

    let report = check(&db.store(), Some(&dir.join("scratch")), false).unwrap();

    assert_eq!(report.findings(), 0, "{report:?}");
    assert_eq!((report.replayed, report.cut_after_redaction), (0, 1));
    done(&dir);
}

#[test]
fn a_cut_tool_preview_whose_argument_was_redacted_is_counted_not_replayed() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("preview_redacted");
    // The card falls inside the preview; its replacement has another length,
    // so the archived (redacted) argument would cut elsewhere.
    let tail = "z".repeat(TOOL_INPUT_CHARS);
    capture(
        &db,
        &[
            text(1, "user", "run it"),
            bash(2, &format!("echo {CARD} {tail}")),
        ],
    );
    // A long command with no redaction still replays.
    capture_as(
        &db,
        "sess_plain",
        &[
            text(1, "user", "run it"),
            bash(2, &format!("echo {tail} {tail}")),
        ],
    );

    let report = check(&db.store(), Some(&dir.join("scratch")), false).unwrap();

    assert_eq!(report.findings(), 0, "{report:?}");
    assert_eq!((report.replayed, report.cut_after_redaction), (1, 1));
    done(&dir);
}

#[test]
fn a_changed_message_tools_setting_is_a_mismatch_and_named() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("tools");
    capture(&db, &[text(1, "user", "ask"), posted(2, "the full reply")]);
    test_env::set_var(MESSAGE_TOOLS_ENV, "");

    let scratch = dir.join("scratch");
    let report = check(&db.store(), Some(&scratch), false).unwrap();

    assert_eq!(report.replay_mismatch.len(), 1, "{report:?}");
    assert_eq!(report.message_tools.as_deref(), Some(""));
    assert!(
        report.kept && scratch.exists(),
        "a failing check keeps its scratch"
    );
    // It holds conversation bodies: owner-only, and everything in it is
    // reached through it.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&scratch).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    done(&dir);
}

#[test]
fn a_skeleton_is_checked_against_the_replayed_lines() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("sketch");
    // Kept in full while the tool is a message tool: ten lines.
    let reply = (1..=10)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let capture_id = capture(&db, &[text(1, "user", "ask"), posted(2, &reply)]);
    let live = Memories::new(&db.store()).all_live().unwrap();
    let lines = live
        .iter()
        .find(|m| m.category == "dialog")
        .map(|m| m.content.lines().count())
        .unwrap();
    sketch(&db, &capture_id, (1, lines)); // holds on the live lines
                                          // Replayed without the message tool, the reply is a short preview.
    test_env::set_var(MESSAGE_TOOLS_ENV, "");

    let report = check(&db.store(), Some(&dir.join("scratch")), false).unwrap();

    assert_eq!(report.skeletons_replayed, 1);
    assert_eq!(report.bad_skeleton, [capture_id], "{report:?}");
    done(&dir);
}

#[test]
fn an_existing_scratch_is_refused() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, db) = fresh("exists");
    let scratch = dir.join("scratch");
    std::fs::create_dir(&scratch).unwrap();

    assert!(check(&db.store(), Some(&scratch), false).is_err());
    done(&dir);
}
