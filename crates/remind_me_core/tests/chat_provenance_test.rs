//! A Claude Code transcript import keeps what the envelopes say: when, which
//! session, where, and one line per tool call.

use remind_me_core::db::memories::Memories;
use remind_me_core::db::sessions::Sessions;
use remind_me_core::importer::{extract_messages_with_tools, import_chat};
use remind_me_core::{ChatImportInput, Database, ImportKind, ImportOutcome, Memory};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap())
        .join(format!("rrm_chatprov_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn envelope(role: &str, ts: &str, content: serde_json::Value) -> String {
    serde_json::json!({
        "type": role,
        "sessionId": "sess-1",
        "uuid": format!("uuid-{ts}"),
        "timestamp": ts,
        "cwd": "/work/quokka",
        "gitBranch": "feat/x",
        "message": {"role": role, "content": content}
    })
    .to_string()
}

fn fixture() -> String {
    [
        envelope(
            "user",
            "2026-10-04T09:00:00.000Z",
            serde_json::json!("fix the flaky test"),
        ),
        envelope(
            "assistant",
            "2026-10-04T09:00:05.000Z",
            serde_json::json!([
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "running the suite"},
                {"type": "tool_use", "id": "t1", "name": "Bash",
                 "input": {"command": "cargo test -p remind_me_core"}}
            ]),
        ),
    ]
    .join("\n")
}

fn run(db: &Database, path: &std::path::Path, mode: &str) -> ImportOutcome {
    import_chat(
        &db.store(),
        &ChatImportInput {
            file_path: path.display().to_string(),
            category: "chat_import".into(),
            tags: vec![],
            extract_mode: mode.into(),
            max_length: 10_000,
            kind: ImportKind::Auto,
        },
    )
    .unwrap()
}

fn memories(db: &Database) -> Vec<Memory> {
    let mut all = Memories::new(&db.store()).all_live().unwrap();
    all.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    all
}

#[test]
fn messages_carry_time_session_place_and_tool_summaries() {
    let line = envelope(
        "assistant",
        "2026-10-04T09:00:05.000Z",
        serde_json::json!([
            {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "x".repeat(300)}},
            {"type": "tool_result", "tool_use_id": "t1", "is_error": true, "content": "boom"}
        ]),
    );
    let messages = extract_messages_with_tools(&serde_json::from_str(&line).unwrap());
    assert_eq!(messages.len(), 1, "a tool-only message is kept");
    let m = &messages[0];
    assert_eq!(m.timestamp.as_deref(), Some("2026-10-04T09:00:05.000Z"));
    assert_eq!(m.session_id.as_deref(), Some("sess-1"));
    assert_eq!(m.cwd.as_deref(), Some("/work/quokka"));
    assert_eq!(m.git_branch.as_deref(), Some("feat/x"));
    assert_eq!(m.tool_use.len(), 1);
    assert_eq!(m.tool_use[0].input_summary.chars().count(), 200);
    assert!(m.tool_use[0].is_error);
}

#[test]
fn an_import_stamps_each_memory_and_is_idempotent() {
    let dir = scratch("stamp");
    let path = dir.join("session.jsonl");
    std::fs::write(&path, fixture()).unwrap();
    let db = Database::open_in_memory().unwrap();

    run(&db, &path, "all_messages");
    let rows = memories(&db);
    assert_eq!(rows.len(), 2);
    let first = &rows[0];
    assert_eq!(first.session_id.as_deref(), Some("sess-1"));
    assert_eq!(first.cwd.as_deref(), Some("/work/quokka"));
    assert_eq!(first.git_branch.as_deref(), Some("feat/x"));
    assert_eq!(first.project.as_deref(), Some("quokka"));
    assert_eq!(first.written_by, "importer:chat");
    assert_eq!(first.capture_method, "auto");
    assert_eq!(
        first.created_at, "2026-10-04T09:00:00.000Z",
        "the message's own time"
    );
    assert_ne!(first.updated_at, first.created_at);
    assert!(
        !rows[1].content.contains("cargo test"),
        "tool blocks are not stored in this mode"
    );
    assert!(Sessions::new(&db.store()).get("sess-1").unwrap().is_some());

    let again = run(&db, &path, "all_messages");
    assert!(matches!(again, ImportOutcome::Skipped { .. }), "{again:?}");
    assert_eq!(memories(&db).len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tool_calls_are_summarised_in_the_conversations_mode_only() {
    let dir = scratch("tools");
    let path = dir.join("session.json");
    let messages: Vec<serde_json::Value> = fixture()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["message"].clone())
        .collect();
    std::fs::write(
        &path,
        serde_json::json!([{ "messages": messages }]).to_string(),
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();

    run(&db, &path, "conversations");
    let rows = memories(&db);
    assert_eq!(rows.len(), 1);
    assert!(rows[0]
        .content
        .contains("tool: Bash — cargo test -p remind_me_core"));
    assert!(!rows[0].content.contains("hmm"), "thinking is not stored");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_chat_without_envelope_fields_is_still_written_by_the_importer() {
    let dir = scratch("plain");
    let path = dir.join("chat.json");
    std::fs::write(&path, r#"[{"role":"user","content":"hi"}]"#).unwrap();
    let db = Database::open_in_memory().unwrap();
    run(&db, &path, "all_messages");
    let rows = memories(&db);
    assert_eq!(rows[0].session_id, None);
    assert_eq!(rows[0].written_by, "importer:chat");
    let _ = std::fs::remove_dir_all(&dir);
}
