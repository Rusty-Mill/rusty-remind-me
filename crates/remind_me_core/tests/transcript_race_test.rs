//! Captures from two writers at once: the hook (through the store's lock)
//! and the folder watcher (from its own thread, without it).
//!
//! Each test stops the watcher between reading the session's capture and
//! writing it, and lets the hook capture during that window. Unserialized,
//! the watcher would then write from a stale reading: a second pair on a
//! first capture, or an older transcript over the hook's newer one. The
//! window is held open until the hook finishes or 300ms pass, so the
//! interleaving that breaks an unserialized capture is the one that runs.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::archive::{self, ARCHIVE_DIR_ENV};
use remind_me_core::db::Store;
use remind_me_core::episodes::session_memories;
use remind_me_core::transcript::{
    capture_transcript, capture_unless_held_pausing, captured_messages, Conditional, Replace,
    TranscriptRequest,
};
use remind_me_core::Database;
use std::sync::mpsc;
use std::time::Duration;

/// A transcript of `n` alternating turns of session `session`.
fn transcript(session: &str, n: usize) -> String {
    (1..=n)
        .map(|i| {
            let role = if i % 2 == 1 { "user" } else { "assistant" };
            format!(
                "{}\n",
                serde_json::json!({"type": role, "sessionId": session,
                    "message": {"role": role, "content": format!("turn {i}")}})
            )
        })
        .collect()
}

fn request<'a>(session: &'a str, reason: &'a str) -> TranscriptRequest<'a> {
    TranscriptRequest {
        session_id: session,
        cwd: None,
        reason: Some(reason),
        context: None,
    }
}

/// A file-backed store, so the watcher's lock-free store can be made, with
/// raw-transcript retention on.
fn setup(name: &str) -> (tempdir::Scratch, Database) {
    let dir = tempdir::Scratch::new(name);
    test_env::set_var(ARCHIVE_DIR_ENV, dir.0.join("archive"));
    let db = Database::open(dir.0.join("memory.db")).unwrap();
    (dir, db)
}

mod tempdir {
    pub struct Scratch(pub std::path::PathBuf);
    impl Scratch {
        pub fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("rrm_race_{name}_{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// `REMIND_ME_ARCHIVE_DIR` is process-global.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run the watcher's capture of `watcher_raw` and, inside its window, the
/// hook's capture of `hook_raw`. Returns what the watcher did.
fn race(db: &Database, session: &str, watcher_raw: String, hook_raw: String) -> Conditional {
    let source = db.secondary_source().unwrap();
    let (looked_up_tx, looked_up_rx) = mpsc::channel::<()>();
    let (hook_done_tx, hook_done_rx) = mpsc::channel::<()>();
    let hook_done_rx = std::sync::Mutex::new(hook_done_rx);

    std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            let store = source.store();
            let pause = || {
                looked_up_tx.send(()).unwrap();
                let _ = hook_done_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_millis(300));
            };
            capture_unless_held_pausing(
                &store,
                &watcher_raw,
                &request(session, "watch"),
                Replace::IfLonger,
                &pause,
            )
            .unwrap()
        });
        looked_up_rx.recv().unwrap();
        capture_transcript(&db.store(), &hook_raw, &request(session, "stop")).unwrap();
        let _ = hook_done_tx.send(());
        watcher.join().unwrap()
    })
}

/// The session holds exactly one dialog and one summary, and the archived
/// transcript is the one behind the dialog that survived.
fn assert_one_consistent_pair(store: &Store<'_>, session: &str, messages: usize) {
    let memories = session_memories(store, session).unwrap();
    let kinds: Vec<&str> = memories
        .iter()
        .filter_map(|m| m.metadata.get("type").and_then(|t| t.as_str()))
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "dialog").count(),
        1,
        "{kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| **k == "summary").count(),
        1,
        "{kinds:?}"
    );
    assert_eq!(captured_messages(store, session).unwrap(), Some(messages));

    let dialog = memories
        .iter()
        .find(|m| m.metadata.get("type").and_then(|t| t.as_str()) == Some("dialog"))
        .unwrap();
    let last = format!("turn {messages}");
    assert!(dialog.content.contains(&last), "{}", dialog.content);
    let source = archive::source_for(store, &dialog.id, true)
        .unwrap()
        .unwrap();
    assert!(source.content.contains(&last));
    assert!(!source.content.contains(&format!("turn {}", messages + 1)));
}

#[test]
fn concurrent_first_captures_make_one_pair() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, db) = setup("first");

    let watched = race(
        &db,
        "s-first",
        transcript("s-first", 3),
        transcript("s-first", 5),
    );

    assert!(matches!(watched, Conditional::First(_)), "{watched:?}");
    assert_one_consistent_pair(&db.store(), "s-first", 5);
}

#[test]
fn a_stale_watcher_reading_never_lands_over_a_newer_hook_capture() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, db) = setup("stale");
    capture_transcript(
        &db.store(),
        &transcript("s-stale", 8),
        &request("s-stale", "stop"),
    )
    .unwrap();

    // The watcher read 10 messages and sees the hook's 8; the hook then
    // captures 12 inside the watcher's window.
    let watched = race(
        &db,
        "s-stale",
        transcript("s-stale", 10),
        transcript("s-stale", 12),
    );

    assert!(matches!(watched, Conditional::Refreshed(_)), "{watched:?}");
    assert_one_consistent_pair(&db.store(), "s-stale", 12);
}

#[test]
fn the_watcher_leaves_a_capture_at_least_as_long_alone() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, db) = setup("held");
    capture_transcript(
        &db.store(),
        &transcript("s-held", 12),
        &request("s-held", "end"),
    )
    .unwrap();

    let held = capture_unless_held_pausing(
        &db.secondary_source().unwrap().store(),
        &transcript("s-held", 10),
        &request("s-held", "watch"),
        Replace::IfLonger,
        &|| {},
    )
    .unwrap();

    assert_eq!(held, Conditional::Held(12));
    assert_one_consistent_pair(&db.store(), "s-held", 12);
}
