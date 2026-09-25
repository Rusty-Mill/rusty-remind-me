//! Readwise highlight import (gap I5, issue #150).
//!
//! No network anywhere in here, deliberately: the reference makes no live call
//! against Readwise and neither does this. A user exports once and hands the
//! importer the saved file. My original issue described an API client with
//! pagination and a stub server — that was wrong, and correcting it before
//! writing code is why these tests are file-shaped.

use remind_me_core::models::ImportKind;
use remind_me_core::readwise_import::{parse_export, READWISE_FORMAT_ERROR};
use remind_me_core::Database;
use serde_json::json;

fn export(entries: serde_json::Value) -> String {
    json!({ "count": 1, "nextPageCursor": null, "results": entries }).to_string()
}

fn one_book(highlights: serde_json::Value) -> serde_json::Value {
    json!([{
        "title": "Thinking, Fast and Slow",
        "author": "Daniel Kahneman",
        "category": "books",
        "source_url": "https://example.com/book",
        "user_book_id": 1234,
        "highlights": highlights,
    }])
}

// ---------------------------------------------------------------------------
// Input shapes
// ---------------------------------------------------------------------------

#[test]
fn the_documented_results_object_parses() {
    let raw = export(one_book(json!([{ "text": "System 1 is fast." }])));
    let (highlights, count) = parse_export(&raw, 2000).unwrap();

    assert_eq!(count, 1);
    assert_eq!(highlights[0].content, "System 1 is fast.");
}

#[test]
fn a_bare_array_parses_too() {
    // Someone may reasonably have unwrapped `results` before saving. Refusing
    // that would reject a file that is obviously a Readwise export.
    let raw = one_book(json!([{ "text": "System 2 is slow." }])).to_string();
    let (highlights, count) = parse_export(&raw, 2000).unwrap();

    assert_eq!(count, 1);
    assert_eq!(highlights[0].content, "System 2 is slow.");
}

#[test]
fn a_file_that_is_not_a_readwise_export_is_refused_with_an_actionable_message() {
    let raw = json!({ "messages": [{ "role": "user", "content": "hi" }] }).to_string();

    // Refused outright rather than imported as zero highlights: partway
    // through is the wrong place to discover the file was never an export,
    // and a silent success is the failure mode #147 fixed elsewhere.
    let err = parse_export(&raw, 2000).unwrap_err();
    assert_eq!(err, READWISE_FORMAT_ERROR);
}

#[test]
fn unparseable_json_says_so_rather_than_claiming_the_wrong_format() {
    let err = parse_export("{not json", 2000).unwrap_err();

    // Two different problems deserve two different messages — "your file is
    // corrupt" sends you somewhere else than "your file is the wrong kind".
    assert!(err.contains("Could not parse Readwise export as JSON"));
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

#[test]
fn a_note_is_appended_to_the_content_not_left_in_metadata() {
    let raw = export(one_book(json!([{
        "text": "Anchoring is powerful.",
        "note": "This explains the salary negotiation result.",
    }])));
    let (highlights, _) = parse_export(&raw, 2000).unwrap();

    // The note is often *why* the highlight was made, and FTS indexes
    // `content`, not `metadata` — metadata-only would make the most valuable
    // part of the record unsearchable.
    assert_eq!(
        highlights[0].content,
        "Anchoring is powerful.\n\nNote: This explains the salary negotiation result."
    );
}

#[test]
fn a_blank_note_adds_no_trailing_marker() {
    let raw = export(one_book(
        json!([{ "text": "Just the passage.", "note": "   " }]),
    ));
    let (highlights, _) = parse_export(&raw, 2000).unwrap();

    assert_eq!(highlights[0].content, "Just the passage.");
}

#[test]
fn one_memory_per_highlight_not_per_book() {
    let raw = export(one_book(json!([
        { "text": "First highlight." },
        { "text": "Second highlight." },
        { "text": "Third highlight." },
    ])));
    let (highlights, count) = parse_export(&raw, 2000).unwrap();

    // Grouped into one memory, every search hit for one highlight would
    // compete for ranking and embedding budget against every other highlight
    // in the same book.
    assert_eq!(count, 3);
    assert_eq!(highlights.len(), 3);
}

#[test]
fn a_highlight_longer_than_the_budget_is_chunked_not_truncated() {
    let long = "sentence. ".repeat(200);
    let raw = export(one_book(json!([{ "text": long }])));
    let (highlights, count) = parse_export(&raw, 200).unwrap();

    // One highlight, several memories. A clipped passage read back later gives
    // no sign it was ever cut.
    assert_eq!(count, 1, "still one highlight");
    assert!(highlights.len() > 1, "got {} chunks", highlights.len());
    // Every chunk keeps the book context; losing it on chunk 2 would make the
    // same passage half-attributed.
    assert!(highlights
        .iter()
        .all(|h| h.metadata["readwise_title"] == "Thinking, Fast and Slow"));
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[test]
fn the_books_context_rides_on_every_highlight() {
    let raw = export(one_book(json!([{
        "text": "A passage.",
        "id": 987,
        "location": 42,
        "location_type": "location",
        "highlighted_at": "2026-01-02T03:04:05Z",
        "url": "https://example.com/highlight",
    }])));
    let meta = &parse_export(&raw, 2000).unwrap().0[0].metadata;

    // The finer grain costs the book as connective tissue; attaching it here
    // pays that back — demoted from "shapes the embedding" to "travels
    // alongside it".
    assert_eq!(meta["readwise_title"], "Thinking, Fast and Slow");
    assert_eq!(meta["readwise_author"], "Daniel Kahneman");
    assert_eq!(meta["readwise_category"], "books");
    assert_eq!(meta["readwise_source_url"], "https://example.com/book");
    assert_eq!(meta["readwise_book_id"], 1234);
    assert_eq!(meta["readwise_highlight_id"], 987);
    assert_eq!(meta["readwise_location"], 42);
    assert_eq!(meta["readwise_location_type"], "location");
    assert_eq!(meta["readwise_highlighted_at"], "2026-01-02T03:04:05Z");
    assert_eq!(meta["readwise_url"], "https://example.com/highlight");
}

#[test]
fn metadata_is_sparse_rather_than_full_of_null_placeholders() {
    let raw = json!([{ "highlights": [{ "text": "Bare minimum." }] }]).to_string();
    let meta = &parse_export(&raw, 2000).unwrap().0[0].metadata;

    // Absent keys let a reader tell "Readwise did not have this" from "this is
    // empty". Null placeholders collapse the two.
    assert!(meta.is_empty(), "got {meta:?}");
}

#[test]
fn a_blank_field_is_omitted_rather_than_stored_as_empty() {
    let raw = json!([{
        "title": "   ",
        "author": "",
        "highlights": [{ "text": "x" }],
    }])
    .to_string();
    let meta = &parse_export(&raw, 2000).unwrap().0[0].metadata;

    assert!(!meta.contains_key("readwise_title"));
    assert!(!meta.contains_key("readwise_author"));
}

#[test]
fn tags_are_flattened_from_objects_to_names() {
    let raw = export(one_book(json!([{
        "text": "Tagged.",
        "tags": [
            { "id": 1, "name": "psychology" },
            { "id": 2, "name": "decision-making" },
            { "id": 3 },
            { "id": 4, "name": "  " },
        ],
    }])));
    let meta = &parse_export(&raw, 2000).unwrap().0[0].metadata;

    // Readwise's `{"id", "name"}` shape is its own bookkeeping; only the names
    // mean anything here, and a nameless or blank one contributes nothing.
    assert_eq!(
        meta["readwise_tags"],
        json!(["psychology", "decision-making"])
    );
}

#[test]
fn an_empty_tag_list_produces_no_key() {
    let raw = export(one_book(json!([{ "text": "x", "tags": [] }])));
    let meta = &parse_export(&raw, 2000).unwrap().0[0].metadata;

    assert!(!meta.contains_key("readwise_tags"));
}

#[test]
fn a_string_id_is_kept_as_a_string() {
    // Readwise sends ids and locations as either, depending on field and
    // export vintage. Both are worth keeping verbatim rather than coerced.
    let raw = export(one_book(
        json!([{ "text": "x", "id": "abc", "location": "page 7" }]),
    ));
    let meta = &parse_export(&raw, 2000).unwrap().0[0].metadata;

    assert_eq!(meta["readwise_highlight_id"], "abc");
    assert_eq!(meta["readwise_location"], "page 7");
}

// ---------------------------------------------------------------------------
// Tolerance for malformed parts
// ---------------------------------------------------------------------------

#[test]
fn malformed_entries_and_highlights_are_skipped_not_fatal() {
    let raw = json!([
        "not an object",
        { "title": "No highlights array" },
        { "highlights": "not an array" },
        { "highlights": [
            "not an object",
            { "note": "no text at all" },
            { "text": "   " },
            { "text": "The one good highlight." },
        ]},
    ])
    .to_string();

    let (highlights, count) = parse_export(&raw, 2000).unwrap();

    // Same tolerance the chat connector shows a bad JSONL line: one bad row
    // must not cost the user the rest of a large export.
    assert_eq!(count, 1);
    assert_eq!(highlights[0].content, "The one good highlight.");
}

// ---------------------------------------------------------------------------
// Routing, end to end
// ---------------------------------------------------------------------------

#[test]
fn an_explicit_readwise_import_stores_highlights_with_their_metadata() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let raw = export(one_book(
        json!([{ "text": "Stored passage.", "note": "why it matters" }]),
    ));

    let outcome = remind_me_core::importer::import_bytes(
        &conn,
        raw.as_bytes(),
        "readwise.json",
        "",
        &[],
        "all_messages",
        2000,
        ImportKind::Readwise,
    )
    .unwrap();
    assert!(
        matches!(
            outcome,
            remind_me_core::models::ImportOutcome::Imported { .. }
        ),
        "got {outcome:?}"
    );

    let (content, category, source, metadata): (String, String, String, String) = conn
        .query_row(
            "SELECT content, category, source, metadata FROM memories",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();

    assert!(content.contains("Stored passage."));
    assert!(content.contains("Note: why it matters"));
    assert_eq!(category, "readwise");
    assert_eq!(source, "readwise_import");

    let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(metadata["readwise_author"], "Daniel Kahneman");
}

#[test]
fn a_json_file_imported_as_auto_is_still_a_chat_import() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let raw = export(one_book(json!([{ "text": "Not reachable from auto." }])));

    remind_me_core::importer::import_bytes(
        &conn,
        raw.as_bytes(),
        "readwise.json",
        "",
        &[],
        "all_messages",
        2000,
        ImportKind::Auto,
    )
    .unwrap();

    // The whole reason Readwise is kept out of auto-detection: a Readwise
    // export and a chat export are both an unadorned `.json`, and sniffing for
    // a `highlights` key would misroute a chat export that merely discusses
    // Readwise — silently corrupting working chat-import behaviour.
    let source: Option<String> = conn
        .query_row("SELECT source FROM memories LIMIT 1", [], |r| r.get(0))
        .ok();
    assert_ne!(source.as_deref(), Some("readwise_import"));
}

#[test]
fn readwise_import_refuses_a_non_json_file() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let outcome = remind_me_core::importer::import_bytes(
        &conn,
        b"# Notes",
        "notes.md",
        "",
        &[],
        "all_messages",
        2000,
        ImportKind::Readwise,
    )
    .unwrap();

    match outcome {
        remind_me_core::models::ImportOutcome::Failed { reason, .. } => {
            assert!(reason.contains("readwise import does not support"))
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_wrong_shaped_json_file_fails_the_import_rather_than_succeeding_emptily() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let outcome = remind_me_core::importer::import_bytes(
        &conn,
        br#"{"messages": []}"#,
        "notachat.json",
        "",
        &[],
        "all_messages",
        2000,
        ImportKind::Readwise,
    )
    .unwrap();

    match outcome {
        remind_me_core::models::ImportOutcome::Failed { reason, .. } => {
            assert!(reason.contains("Not a recognized Readwise export"))
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn every_advertised_kind_string_deserializes() {
    // The tool schema advertises these five by name; a variant that does not
    // deserialize would be advertised and unreachable — the same
    // advertised-but-unroutable failure the tool cross-check exists to catch.
    for (name, expected) in [
        ("auto", ImportKind::Auto),
        ("chat", ImportKind::Chat),
        ("document", ImportKind::Document),
        ("obsidian", ImportKind::Obsidian),
        ("readwise", ImportKind::Readwise),
    ] {
        let parsed: ImportKind =
            serde_json::from_value(serde_json::Value::String(name.to_string()))
                .unwrap_or_else(|e| panic!("{name} did not deserialize: {e}"));
        assert_eq!(parsed, expected, "{name}");
    }
}
