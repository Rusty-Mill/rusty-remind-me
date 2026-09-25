//! Coverage for `remind_me_auto_capture` / `remind_me_get_capture`.

use remind_me_core::capture::{auto_capture, get_capture};
use remind_me_core::db::queries;
use remind_me_core::{
    AutoCaptureInput, CaptureResult, Database, MemorySearchInput, CAPTURE_SOURCE, DIALOG_CATEGORY,
};
use rusqlite::Connection;

fn input(conversation: &str, summary: &str) -> AutoCaptureInput {
    AutoCaptureInput {
        conversation: conversation.to_string(),
        summary: summary.to_string(),
        title: String::new(),
        tags: vec!["session".into()],
        category: "conversation".into(),
        metadata: serde_json::json!({}),
    }
}

fn capture(conn: &Connection, conversation: &str, summary: &str) -> CaptureResult {
    auto_capture(conn, &input(conversation, summary)).unwrap()
}

fn column(conn: &Connection, id: &str, name: &str) -> String {
    conn.query_row(
        &format!("SELECT {} FROM memories WHERE id = ?", name),
        rusqlite::params![id],
        |r| r.get::<_, String>(0),
    )
    .unwrap()
}

fn metadata(conn: &Connection, id: &str) -> serde_json::Value {
    serde_json::from_str(&column(conn, id, "metadata")).unwrap()
}

#[test]
fn a_capture_writes_two_linked_memories() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = capture(&conn, "user: hi\nassistant: hello", "We said hello.");

    assert_ne!(result.dialog_id, result.summary_id);
    assert_eq!(
        column(&conn, &result.dialog_id, "capture_id"),
        result.capture_id
    );
    assert_eq!(
        column(&conn, &result.summary_id, "capture_id"),
        result.capture_id
    );
    assert_eq!(
        column(&conn, &result.dialog_id, "content"),
        "user: hi\nassistant: hello"
    );
    assert_eq!(
        column(&conn, &result.summary_id, "content"),
        "We said hello."
    );
}

#[test]
fn the_dialog_category_is_not_the_callers_category() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut custom = input("the transcript", "the summary");
    custom.category = "meeting".into();

    let result = auto_capture(&conn, &custom).unwrap();

    // `category` names the summary. The dialog is always 'dialog', and
    // `extract_batch` excludes that category — storing a transcript under the
    // caller's category would flood the annotation backlog with raw
    // conversations.
    assert_eq!(
        column(&conn, &result.dialog_id, "category"),
        DIALOG_CATEGORY
    );
    assert_eq!(column(&conn, &result.summary_id, "category"), "meeting");
}

#[test]
fn both_halves_are_stored_under_the_capture_source() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = capture(&conn, "transcript", "summary");

    assert_eq!(column(&conn, &result.dialog_id, "source"), CAPTURE_SOURCE);
    assert_eq!(column(&conn, &result.summary_id, "source"), CAPTURE_SOURCE);
}

#[test]
fn each_half_points_at_the_other() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = capture(&conn, "transcript", "summary");

    let dialog = metadata(&conn, &result.dialog_id);
    let summary = metadata(&conn, &result.summary_id);
    assert_eq!(dialog["type"], "dialog");
    assert_eq!(summary["type"], "summary");
    // The dialog's pointer is the interesting one: the summary's id does not
    // exist yet when the dialog row is built, so an implementation that only
    // wrote metadata once would leave this empty.
    assert_eq!(dialog["linked_summary"], result.summary_id);
    assert_eq!(summary["linked_dialog"], result.dialog_id);
    assert_eq!(dialog["capture_id"], result.capture_id);
    assert_eq!(summary["capture_id"], result.capture_id);
}

#[test]
fn the_title_falls_back_to_the_summarys_first_line() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = capture(
        &conn,
        "transcript",
        "We chose SQLite.\nMore detail follows.",
    );

    assert_eq!(result.title, "We chose SQLite.");
    assert_eq!(
        metadata(&conn, &result.dialog_id)["title"],
        "We chose SQLite."
    );
}

#[test]
fn a_long_first_line_is_capped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = capture(&conn, "transcript", &"x".repeat(200));

    assert_eq!(result.title.chars().count(), 80);
}

#[test]
fn a_supplied_title_wins() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut titled = input("transcript", "the summary");
    titled.title = "Design review".into();

    let result = auto_capture(&conn, &titled).unwrap();

    assert_eq!(result.title, "Design review");
}

#[test]
fn caller_metadata_is_preserved_alongside_the_link_fields() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mut with_meta = input("transcript", "summary");
    with_meta.metadata = serde_json::json!({ "project": "rusty" });

    let result = auto_capture(&conn, &with_meta).unwrap();

    let dialog = metadata(&conn, &result.dialog_id);
    assert_eq!(dialog["project"], "rusty");
    assert_eq!(dialog["type"], "dialog");
}

#[test]
fn both_halves_carry_the_tags() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let result = capture(&conn, "transcript", "summary");

    assert_eq!(column(&conn, &result.dialog_id, "tags"), r#"["session"]"#);
    assert_eq!(column(&conn, &result.summary_id, "tags"), r#"["session"]"#);
}

#[test]
fn a_capture_is_searchable() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // Both halves must contain the term verbatim — the FTS tokenizer does not
    // stem, so "quokkas" would not match a search for "quokka".
    let result = capture(
        &conn,
        "we discussed the quokka at length",
        "quokka decision",
    );

    let found: Vec<String> = queries::search_memories(
        &conn,
        &MemorySearchInput {
            strategy: Default::default(),
            include_sensitive: false,
            query: "quokka".into(),
            category: None,
            tags: None,
            limit: 20,
            token_budget: 100_000,
            response_format: Default::default(),
            include_dormant: true,
            min_vitality: 0.0,
            verbose: false,
            expand_entities: false,
            include_neighbors: false,
            expand_co_retrieval: false,
            bootstrap: false,
        },
    )
    .unwrap()
    .into_iter()
    .map(|r| r.memory.id)
    .collect();

    // Both halves are ordinary memories, so the FTS triggers must have picked
    // them up.
    assert!(found.contains(&result.dialog_id));
    assert!(found.contains(&result.summary_id));
}

#[test]
fn the_pair_comes_back_by_capture_id() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let result = capture(&conn, "the transcript", "the summary");

    let found = get_capture(&conn, &result.capture_id).unwrap().unwrap();

    assert_eq!(found.capture_id, result.capture_id);
    assert_eq!(found.dialog.as_ref().unwrap().id, result.dialog_id);
    assert_eq!(found.summary.as_ref().unwrap().id, result.summary_id);
    assert_eq!(found.title, result.title);
    assert!(found.other.is_empty());
}

#[test]
fn an_unknown_capture_id_is_none() {
    let db = Database::open_in_memory().unwrap();
    assert!(get_capture(&db.conn(), "cap_nope").unwrap().is_none());
}

#[test]
fn two_captures_do_not_share_an_id() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let first = capture(&conn, "same text", "same summary");
    let second = capture(&conn, "same text", "same summary");

    // Identical content is two captures, not an upsert.
    assert_ne!(first.capture_id, second.capture_id);
    let found = get_capture(&conn, &first.capture_id).unwrap().unwrap();
    assert_eq!(found.dialog.unwrap().id, first.dialog_id);
}

#[test]
fn a_half_lost_to_deletion_still_reports_the_other() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let result = capture(&conn, "the transcript", "the summary");
    queries::delete_memory(&conn, &result.dialog_id).unwrap();

    let found = get_capture(&conn, &result.capture_id).unwrap().unwrap();

    assert!(found.dialog.is_none());
    assert_eq!(found.summary.unwrap().id, result.summary_id);
}

#[test]
fn an_extra_row_sharing_the_id_is_surfaced_not_dropped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let result = capture(&conn, "the transcript", "the summary");
    // Sync can deliver a third row carrying the same capture_id.
    conn.execute(
        "INSERT INTO memories (id, content, category, tags, source, metadata, capture_id,
                               created_at, updated_at)
         VALUES ('mem_extra', 'stray', 'general', '[]', 'manual', '{}', ?,
                 '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        rusqlite::params![result.capture_id],
    )
    .unwrap();

    let found = get_capture(&conn, &result.capture_id).unwrap().unwrap();

    assert!(found.dialog.is_some());
    assert!(found.summary.is_some());
    assert_eq!(
        found.other.len(),
        1,
        "a row that is neither half must be visible, not silently dropped"
    );
}
