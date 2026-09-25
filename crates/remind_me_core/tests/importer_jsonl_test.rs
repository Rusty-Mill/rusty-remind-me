//! Claude Code session transcripts and content-block filtering (issue #147).
//!
//! The failure this covers was silent in the worst way: the import succeeded,
//! reported zero memories created, and looked exactly like importing an empty
//! file. Nothing errored and nothing warned, so the only way to notice was to
//! go looking for content that should have been there.

use remind_me_core::importer::extract_messages;
use serde_json::json;

/// One Claude Code transcript line: an envelope naming the speaker, with the
/// real message one level down under `message`.
fn envelope(kind: &str, role: &str, content: serde_json::Value) -> serde_json::Value {
    json!({ "type": kind, "message": { "role": role, "content": content } })
}

#[test]
fn a_claude_code_envelope_is_unwrapped() {
    let line = envelope(
        "assistant",
        "assistant",
        json!([{ "type": "text", "text": "the quokka is on Rottnest Island" }]),
    );

    let messages = extract_messages(&line);

    // Before this branch existed the envelope matched nothing and extraction
    // returned an empty vec — recorded as a successful import of 0 memories.
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "assistant");
    assert_eq!(messages[0].content, "the quokka is on Rottnest Island");
}

#[test]
fn tool_and_thinking_blocks_are_dropped_but_text_survives() {
    let line = envelope(
        "assistant",
        "assistant",
        json!([
            { "type": "thinking", "thinking": "let me check the schema" },
            { "type": "text", "text": "the table is called memories" },
            { "type": "tool_use", "name": "Read", "input": { "text": "not conversation" } },
            { "type": "tool_result", "content": "rows: 3" },
        ]),
    );

    let messages = extract_messages(&line);

    assert_eq!(messages.len(), 1);
    // Machine chatter, not conversation. Kept, it would bury the recallable
    // facts under transcript noise — and `tool_use` here carries a `text` key
    // inside its input, which a type-blind reader would happily import.
    assert_eq!(messages[0].content, "the table is called memories");
}

#[test]
fn a_block_with_no_type_but_a_text_key_is_kept() {
    let line = envelope(
        "user",
        "user",
        json!([{ "text": "older exports omit the discriminator" }]),
    );

    let messages = extract_messages(&line);

    // Dropping these would silently lose real conversation to a format
    // detail — the opposite mistake from importing tool calls.
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "older exports omit the discriminator");
}

#[test]
fn a_message_of_nothing_but_tool_calls_yields_no_blank_entry() {
    let line = envelope(
        "assistant",
        "assistant",
        json!([{ "type": "tool_use", "name": "Bash" }]),
    );

    // Not an empty-content message with a role attached: there is no
    // conversation here at all.
    assert!(extract_messages(&line).is_empty());
}

#[test]
fn text_blocks_are_joined_without_blank_padding() {
    let line = envelope(
        "assistant",
        "assistant",
        json!([
            { "type": "text", "text": "first" },
            { "type": "tool_use", "name": "Bash" },
            { "type": "text", "text": "second" },
        ]),
    );

    let messages = extract_messages(&line);

    // The dropped block must not leave its gap behind. Joined naively this
    // reads "first\n\nsecond", which is a different message.
    assert_eq!(messages[0].content, "first\nsecond");
}

#[test]
fn the_envelope_type_names_the_speaker_when_the_inner_role_is_null() {
    let line = json!({
        "type": "assistant",
        "message": { "role": null, "content": "no inner role" },
    });

    let messages = extract_messages(&line);

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "assistant");
}

// ---------------------------------------------------------------------------
// The guard: other shapes must still fall through unchanged
// ---------------------------------------------------------------------------

#[test]
fn an_object_with_a_message_key_that_is_not_a_message_falls_through() {
    // A `message` key that holds no role is not a transcript envelope. The
    // branch has to be narrow enough that any export happening to use that
    // field name keeps its old behaviour.
    let not_a_transcript = json!({
        "message": { "subject": "release notes", "body": "shipped" },
        "role": "user",
        "content": "the outer object is the message here",
    });

    let messages = extract_messages(&not_a_transcript);

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "the outer object is the message here");
}

#[test]
fn a_chat_messages_export_is_unaffected() {
    let export = json!({
        "chat_messages": [
            { "sender": "human", "content": [{ "type": "text", "text": "hello" }] },
            { "sender": "assistant", "content": [{ "type": "text", "text": "hi" }] },
        ],
        // Present but must not win: `chat_messages` is checked first.
        "message": { "role": "assistant", "content": "wrong answer" },
    });

    let messages = extract_messages(&export);

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "hello");
    assert_eq!(messages[1].content, "hi");
}

#[test]
fn a_graph_record_is_still_skipped() {
    let record = json!({
        "record_type": "entity",
        "message": { "role": "assistant", "content": "not a message" },
    });

    // Entity-graph records are restored separately; parsing one as chat would
    // duplicate it into the vault as prose.
    assert!(extract_messages(&record).is_empty());
}

#[test]
fn a_transcript_of_many_lines_extracts_every_one() {
    // How the importer actually meets this shape: JSONL, one envelope per
    // line, parsed and extracted line by line.
    let lines = [
        envelope("user", "user", json!("what is the capital of Peru")),
        envelope(
            "assistant",
            "assistant",
            json!([{ "type": "text", "text": "Lima" }]),
        ),
        envelope(
            "user",
            "user",
            json!([{ "type": "tool_result", "content": "x" }]),
        ),
    ];

    let extracted: Vec<_> = lines.iter().flat_map(extract_messages).collect();

    // Two of three lines carry conversation; the tool-result line carries none
    // and contributes nothing rather than an empty message.
    assert_eq!(extracted.len(), 2);
    assert_eq!(extracted[0].content, "what is the capital of Peru");
    assert_eq!(extracted[1].content, "Lima");
}
