//! Obsidian vault import (gap I4, issue #149).
//!
//! The extraction gets the attention. Chunking is the existing Markdown
//! chunker and is already covered; what is new here is deciding what in a note
//! is a tag, what is a link, and what is neither — and every one of those
//! mistakes is quiet. A `#` in a code sample becoming a tag, or a heading
//! anchor forking a second entity, both produce a vault that looks fine until
//! you go looking for something by the name you expected it to have.

use remind_me_core::obsidian_import::{
    dedupe_ci, extract_inline_tags, frontmatter_tags, parse_frontmatter, parse_note,
    parse_wikilinks,
};

fn titles(text: &str) -> Vec<String> {
    parse_wikilinks(text).into_iter().map(|l| l.title).collect()
}

fn tags(text: &str) -> Vec<String> {
    extract_inline_tags(text, &[])
}

// ---------------------------------------------------------------------------
// Frontmatter
// ---------------------------------------------------------------------------

#[test]
fn the_three_flat_shapes_parse() {
    let (fields, body) = parse_frontmatter(
        "---\n\
         title: Quokka notes\n\
         tags: [wildlife, australia]\n\
         related:\n  - Rottnest\n  - Perth\n\
         count: 3\n\
         draft: false\n\
         ---\n\
         The body.\n",
    );

    assert_eq!(fields["title"], "Quokka notes");
    assert_eq!(fields["tags"], serde_json::json!(["wildlife", "australia"]));
    assert_eq!(fields["related"], serde_json::json!(["Rottnest", "Perth"]));
    assert_eq!(fields["count"], 3);
    assert_eq!(fields["draft"], false);
    assert_eq!(body, "The body.\n");
}

#[test]
fn unparseable_frontmatter_still_imports_the_body() {
    let (fields, body) = parse_frontmatter(
        "---\n\
         nested:\n  inner: value\n\
         ---\n\
         The prose survives.\n",
    );

    // Degrade to "no fields", never an error. Partial extraction would be
    // worse than none, because a caller cannot tell which half it got — and
    // the note's prose is the part that actually matters.
    assert!(fields.is_empty());
    assert_eq!(body, "The prose survives.\n");
}

#[test]
fn an_anchor_or_flow_mapping_also_degrades_rather_than_half_parsing() {
    for exotic in ["ref: &anchor", "flow: {a: 1}", "alias: *ref"] {
        let (fields, _) = parse_frontmatter(&format!("---\nok: yes\n{}\n---\nbody\n", exotic));
        assert!(fields.is_empty(), "{exotic} should degrade the whole block");
    }
}

#[test]
fn a_note_starting_with_a_horizontal_rule_is_not_frontmatter() {
    let text = "---\nJust a rule, no closing delimiter.\n";
    let (fields, body) = parse_frontmatter(text);

    // Unterminated means it was never a frontmatter block, so nothing may be
    // stripped — the `---` is part of the note.
    assert!(fields.is_empty());
    assert_eq!(body, text);
}

#[test]
fn a_note_with_no_frontmatter_is_left_alone() {
    let (fields, body) = parse_frontmatter("# Heading\n\nProse.\n");
    assert!(fields.is_empty());
    assert_eq!(body, "# Heading\n\nProse.\n");
}

#[test]
fn frontmatter_tags_accept_both_obsidian_shapes() {
    let (list, _) = parse_frontmatter("---\ntags: [a, b]\n---\nx\n");
    assert_eq!(frontmatter_tags(&list), vec!["a", "b"]);

    let (csv, _) = parse_frontmatter("---\ntags: project, work\n---\nx\n");
    assert_eq!(frontmatter_tags(&csv), vec!["project", "work"]);

    let (single, _) = parse_frontmatter("---\ntags: solo\n---\nx\n");
    assert_eq!(frontmatter_tags(&single), vec!["solo"]);

    let (none, _) = parse_frontmatter("---\ntitle: x\n---\nx\n");
    assert!(frontmatter_tags(&none).is_empty());
}

// ---------------------------------------------------------------------------
// Wikilinks
// ---------------------------------------------------------------------------

#[test]
fn all_three_wikilink_forms_resolve_to_the_note_title() {
    assert_eq!(titles("see [[Rottnest]]"), vec!["Rottnest"]);
    assert_eq!(titles("see [[Rottnest|the island]]"), vec!["Rottnest"]);
    // The v1 limitation, pinned: an anchor resolves to the note as a whole,
    // and must not fork a second "Rottnest#Wildlife" entity.
    assert_eq!(titles("see [[Rottnest#Wildlife]]"), vec!["Rottnest"]);
    assert_eq!(titles("see [[Rottnest^abc123]]"), vec!["Rottnest"]);
    assert_eq!(
        titles("see [[Rottnest#Wildlife|quokkas]]"),
        vec!["Rottnest"]
    );
}

#[test]
fn every_occurrence_is_returned_in_order_not_deduplicated() {
    // A caller deciding which chunk a mention landed in needs each occurrence
    // separately; the unique set is one dedupe away.
    assert_eq!(
        titles("[[A]] then [[B]] then [[A]] again"),
        vec!["A", "B", "A"]
    );
}

#[test]
fn an_unclosed_wikilink_does_not_swallow_the_note() {
    assert!(titles("[[Unclosed\nNext line is prose").is_empty());
}

#[test]
fn an_empty_or_anchor_only_target_is_not_a_link() {
    assert!(titles("[[]]").is_empty());
    assert!(titles("[[#Heading]]").is_empty());
}

// ---------------------------------------------------------------------------
// Inline tags
// ---------------------------------------------------------------------------

#[test]
fn a_markdown_heading_is_not_a_tag() {
    // The space after `#` is the whole distinction, and getting it wrong turns
    // every heading in the vault into a tag.
    assert!(tags("# Heading\n## Another").is_empty());
}

#[test]
fn a_wikilink_anchor_is_not_a_tag() {
    let text = "see [[Note#Heading]]";
    let spans: Vec<(usize, usize)> = parse_wikilinks(text)
        .iter()
        .map(|l| (l.start, l.end))
        .collect();

    assert!(extract_inline_tags(text, &spans).is_empty());
}

#[test]
fn a_hash_inside_code_is_not_a_tag() {
    assert!(tags("run `#!/bin/sh` first").is_empty());
    assert!(tags("```sh\n# comment\n#nottag\n```\n").is_empty());
    // An unterminated fence runs to the end rather than leaking its contents.
    assert!(tags("```\n#nottag\n").is_empty());
}

#[test]
fn a_purely_numeric_tag_is_dropped() {
    // Obsidian does not allow one, and `#123` is far more likely to be an
    // issue reference than a tag.
    assert!(tags("closes #123").is_empty());
    // Only *purely* numeric. `#2024/review` is a real nested tag — a year
    // bucket — and dropping it would lose the most common dated-note scheme
    // there is. The slash is stripped before the digit check precisely so the
    // check answers "is this a bare number" rather than "does it contain one".
    assert_eq!(tags("#2024/review"), vec!["2024/review"]);
    assert!(tags("#2024/2025").is_empty(), "still purely numeric");
    // Mixed is fine.
    assert_eq!(tags("#v2"), vec!["v2"]);
}

#[test]
fn nested_and_hyphenated_tags_are_kept_whole() {
    assert_eq!(
        tags("#project/alpha and #long-name"),
        vec!["project/alpha", "long-name"]
    );
}

#[test]
fn tags_are_deduplicated_case_insensitively_keeping_first_casing() {
    assert_eq!(
        tags("#Project then #project then #PROJECT"),
        vec!["Project"]
    );
}

#[test]
fn a_hash_mid_word_is_not_a_tag() {
    assert!(tags("colour#ff0000 and C# too").is_empty());
}

// ---------------------------------------------------------------------------
// The connector
// ---------------------------------------------------------------------------

#[test]
fn frontmatter_and_inline_tags_combine_without_duplicates() {
    let (chunks, _) = parse_note(
        "---\ntags: [wildlife, Australia]\n---\nQuokkas live here. #australia #island\n",
        2000,
    );

    assert_eq!(chunks.len(), 1);
    // `Australia` from frontmatter and `#australia` inline are the same tag.
    assert_eq!(
        chunks[0].extra_tags,
        vec!["wildlife", "Australia", "island"]
    );
}

#[test]
fn frontmatter_minus_tags_is_kept_as_metadata() {
    let (_, frontmatter) = parse_note("---\ntitle: Notes\ntags: [a]\n---\nbody\n", 2000);

    assert_eq!(frontmatter["title"], "Notes");
    // `tags` is folded into the memory's tags, so keeping it here too would
    // store the same fact twice in two places that can drift apart.
    assert!(!frontmatter.contains_key("tags"));
}

#[test]
fn a_mention_attaches_to_the_chunk_that_made_it() {
    let note = "# First\n\nMentions [[Alpha]] here.\n\n# Second\n\nMentions [[Beta]] here.\n";
    let (chunks, _) = parse_note(note, 2000);

    assert!(chunks.len() >= 2, "expected one chunk per section");
    let first = chunks
        .iter()
        .find(|c| c.content.contains("Alpha"))
        .expect("a chunk containing the Alpha mention");
    let second = chunks
        .iter()
        .find(|c| c.content.contains("Beta"))
        .expect("a chunk containing the Beta mention");

    // Smeared across the note, every section would claim every mention and the
    // graph would say each section is about everything the file touches.
    assert_eq!(first.mention_entities, vec!["Alpha"]);
    assert_eq!(second.mention_entities, vec!["Beta"]);
}

#[test]
fn a_note_with_no_links_or_tags_still_chunks() {
    let (chunks, frontmatter) = parse_note("Just prose, nothing special.\n", 2000);

    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].extra_tags.is_empty());
    assert!(chunks[0].mention_entities.is_empty());
    assert!(frontmatter.is_empty());
}

#[test]
fn dedupe_ci_keeps_order_and_first_casing() {
    assert_eq!(
        dedupe_ci(["Beta".to_string(), "beta".to_string(), "Alpha".to_string()]),
        vec!["Beta", "Alpha"]
    );
    // Empty strings are dropped rather than becoming a blank tag.
    assert_eq!(dedupe_ci([String::new(), "x".to_string()]), vec!["x"]);
}

// ---------------------------------------------------------------------------
// End to end, through the real ingest path
// ---------------------------------------------------------------------------
//
// The unit tests above cover extraction. This covers the half that only the
// ingest path does — merging tags with the caller's, writing frontmatter to
// metadata, and turning mentions into linked entities. Asserting each side
// separately is exactly how the `sensitive` sync bug got through once, so the
// join is asserted here too.

use remind_me_core::models::ImportKind;
use remind_me_core::Database;

#[test]
fn a_note_imports_with_merged_tags_frontmatter_and_linked_entities() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let note = "---\n\
                title: Island notes\n\
                tags: [wildlife]\n\
                ---\n\
                Quokkas live on [[Rottnest]]. #australia\n";

    let outcome = remind_me_core::importer::import_bytes(
        &conn,
        note.as_bytes(),
        "island.md",
        "",
        &["caller-tag".to_string()],
        "all_messages",
        2000,
        ImportKind::Obsidian,
    )
    .unwrap();

    assert!(
        matches!(
            outcome,
            remind_me_core::models::ImportOutcome::Imported { .. }
        ),
        "got {outcome:?}"
    );

    let (content, category, source, tags_json, metadata): (String, String, String, String, String) =
        conn.query_row(
            "SELECT content, category, source, tags, metadata FROM memories",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();

    assert!(content.contains("Quokkas live on"));
    assert_eq!(
        category, "obsidian",
        "its own category, not the document one"
    );
    assert_eq!(source, "obsidian_import");

    // The caller's tag is kept, not replaced: they asked for these memories to
    // be tagged that way, and the note's own tags are additional information.
    let stored: Vec<String> = serde_json::from_str(&tags_json).unwrap();
    assert!(stored.contains(&"caller-tag".to_string()));
    assert!(stored.contains(&"wildlife".to_string()));
    assert!(stored.contains(&"australia".to_string()));

    let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(metadata["obsidian_frontmatter"]["title"], "Island notes");

    // The mention became a real, traversable entity link — the whole point of
    // treating a wikilink as more than text.
    let linked: String = conn
        .query_row(
            "SELECT e.name FROM entities e
               JOIN memory_entities me ON me.entity_id = e.id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(linked, "Rottnest");
}

#[test]
fn a_link_to_a_note_that_does_not_exist_yet_still_resolves() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // Entity upsert creates what is missing, so a forward reference into a
    // vault whose other notes have not been imported needs no special case.
    remind_me_core::importer::import_bytes(
        &conn,
        b"Refers to [[Not Yet Imported]].\n",
        "note.md",
        "",
        &[],
        "all_messages",
        2000,
        ImportKind::Obsidian,
    )
    .unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE name = 'Not Yet Imported'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn re_importing_the_same_note_is_a_no_op() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let note = b"Some vault note with [[A Link]].\n";

    for _ in 0..2 {
        remind_me_core::importer::import_bytes(
            &conn,
            note,
            "note.md",
            "",
            &[],
            "all_messages",
            2000,
            ImportKind::Obsidian,
        )
        .unwrap();
    }

    let memories: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        memories, 1,
        "the content hash short-circuits the second run"
    );
}

#[test]
fn obsidian_import_refuses_a_non_markdown_file() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let outcome = remind_me_core::importer::import_bytes(
        &conn,
        b"plain text",
        "notes.txt",
        "",
        &[],
        "all_messages",
        2000,
        ImportKind::Obsidian,
    )
    .unwrap();

    // The conventions this connector understands are Markdown conventions.
    // Accepting `.txt` would silently do nothing useful with it.
    match outcome {
        remind_me_core::models::ImportOutcome::Failed { reason, .. } => {
            assert!(reason.contains("obsidian import does not support"))
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}
