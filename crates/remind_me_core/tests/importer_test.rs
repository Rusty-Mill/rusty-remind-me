//! Coverage for `remind_me_import_chat` / `remind_me_import_directory`.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::import_paths::{validate_import_dir, validate_import_file, ImportPathError};
use remind_me_core::importer::{
    chunk_text, import_chat, import_directory, looks_like_chat_markdown, parse_document,
    split_markdown_sections, CHAT_SOURCE, DOCUMENT_CATEGORY, DOCUMENT_SOURCE,
};
use remind_me_core::sync::{CLIENT_ENV, NODE_ID_ENV};
use remind_me_core::{
    BulkImportDirInput, ChatImportInput, Database, ImportKind, ImportOutcome, NormalizeBatchInput,
};
use rusqlite::Connection;
use std::sync::Mutex;

/// A scratch directory inside the default import root (the home directory).
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap())
        .join(format!("rrm_import_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

fn import(
    conn: &Connection,
    path: &std::path::Path,
    configure: impl FnOnce(&mut ChatImportInput),
) -> ImportOutcome {
    let mut input = ChatImportInput {
        file_path: path.display().to_string(),
        category: "chat_import".into(),
        tags: vec!["imported".into()],
        extract_mode: "assistant_messages".into(),
        max_length: 10_000,
        kind: ImportKind::Auto,
    };
    configure(&mut input);
    import_chat(conn, &input).unwrap()
}

fn contents(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT content FROM memories ORDER BY chunk_index, id")
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn column(conn: &Connection, name: &str) -> String {
    conn.query_row(&format!("SELECT {} FROM memories LIMIT 1", name), [], |r| {
        r.get::<_, String>(0)
    })
    .unwrap()
}

const CHAT_JSON: &str = r#"[
  {"role": "user", "content": "what is a quokka"},
  {"role": "assistant", "content": "a small marsupial"}
]"#;

// --- chunking ----------------------------------------------------------------

#[test]
fn short_text_is_one_chunk() {
    assert_eq!(chunk_text("hello", 100), vec!["hello".to_string()]);
    assert!(chunk_text("   ", 100).is_empty());
}

#[test]
fn chunking_prefers_paragraph_then_line_then_sentence() {
    let paragraphs = format!("{}\n\n{}", "a".repeat(30), "b".repeat(30));
    assert_eq!(chunk_text(&paragraphs, 40).len(), 2);

    let sentences = format!("{}. {}", "a".repeat(30), "b".repeat(30));
    let chunks = chunk_text(&sentences, 40);
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].ends_with('.'));
}

#[test]
fn chunking_falls_back_to_a_hard_cut() {
    let unbroken = "x".repeat(250);
    let chunks = chunk_text(&unbroken, 100);
    assert_eq!(chunks.len(), 3);
    assert!(chunks.iter().all(|c| c.chars().count() <= 100));
}

#[test]
fn a_whitespace_only_window_is_not_stored() {
    // A long run of indentation strips to nothing and must not become a blank
    // memory.
    let padded = format!("{}\n{}", " ".repeat(200), "real content");
    let chunks = chunk_text(&padded, 50);
    assert!(chunks.iter().all(|c| !c.trim().is_empty()));
}

#[test]
fn chunking_does_not_split_a_multibyte_character() {
    let text = "é".repeat(250);
    let chunks = chunk_text(&text, 100);
    // Slicing on a byte offset would panic; every chunk must be valid UTF-8
    // of the expected character length.
    assert!(chunks.iter().all(|c| c.chars().count() <= 100));
    assert_eq!(chunks.concat().chars().count(), 250);
}

// --- chat parsing ------------------------------------------------------------

#[test]
fn a_json_chat_export_imports_assistant_turns_by_default() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("json");
    let path = write(&dir, "chat.json", CHAT_JSON);

    let outcome = import(&conn, &path, |_| {});

    assert!(matches!(outcome, ImportOutcome::Imported { .. }));
    assert_eq!(contents(&conn), vec!["a small marsupial".to_string()]);
    assert_eq!(column(&conn, "source"), CHAT_SOURCE);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// `REMIND_ME_NODE_ID`/`REMIND_ME_CLIENT` are process-global; no other test
/// in this file touches them, so this is the only one that needs the guard.
static PROVENANCE_ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn a_chat_import_is_stamped_with_the_configured_node_and_client() {
    // #266: `import_content` -- the shared writer behind `import_chat`,
    // `import_directory`, and a webhook push -- never stamped `node_id`/
    // `client` at all, reopening the bug #258 fixed for six other write
    // paths. This is the chat-import fixture #266 itself asked for.
    let _guard = PROVENANCE_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var(NODE_ID_ENV, "import-test-node");
    crate::test_env::set_var(CLIENT_ENV, "import-test-client");

    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("provenance");
    let path = write(&dir, "chat.json", CHAT_JSON);

    let outcome = import(&conn, &path, |_| {});
    assert!(matches!(outcome, ImportOutcome::Imported { .. }));

    let (node_id, client): (Option<String>, String) = conn
        .query_row("SELECT node_id, client FROM memories LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();

    crate::test_env::remove_var(NODE_ID_ENV);
    crate::test_env::remove_var(CLIENT_ENV);
    std::fs::remove_dir_all(&dir).unwrap();

    assert_eq!(node_id.as_deref(), Some("import-test-node"));
    assert_eq!(client, "import-test-client");
}

#[test]
fn every_extract_mode_selects_what_it_says() {
    let dir = scratch("modes");
    let path = write(&dir, "chat.json", CHAT_JSON);

    for (mode, expected) in [
        ("assistant_messages", vec!["a small marsupial"]),
        ("user_messages", vec!["what is a quokka"]),
        (
            "all_messages",
            vec!["[user] what is a quokka", "[assistant] a small marsupial"],
        ),
        (
            "conversations",
            vec!["**user:** what is a quokka\n\n**assistant:** a small marsupial"],
        ),
        ("summaries", vec![]),
    ] {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        import(&conn, &path, |i| i.extract_mode = mode.to_string());
        let got = contents(&conn);
        assert_eq!(got, expected, "mode {}", mode);
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_claude_export_with_block_content_is_parsed() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("claude");
    let path = write(
        &dir,
        "export.json",
        r#"{"chat_messages": [
             {"sender": "assistant", "content": [{"type": "text", "text": "block one"},
                                                  {"type": "text", "text": "block two"}]}
           ]}"#,
    );

    import(&conn, &path, |_| {});

    assert_eq!(contents(&conn), vec!["block one\nblock two".to_string()]);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_malformed_jsonl_line_does_not_lose_the_rest() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("jsonl");
    let path = write(
        &dir,
        "chat.jsonl",
        "{\"role\":\"assistant\",\"content\":\"first\"}\nnot json at all\n{\"role\":\"assistant\",\"content\":\"second\"}\n",
    );

    import(&conn, &path, |_| {});

    assert_eq!(
        contents(&conn).len(),
        2,
        "one bad line must not fail the import"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- document parsing --------------------------------------------------------

#[test]
fn markdown_sections_carry_a_heading_breadcrumb() {
    let sections = split_markdown_sections(
        "# Projects\n\nintro\n\n## Remind Me\n\ndetail\n\n### Rust\n\nmore",
    );

    let headings: Vec<Option<String>> = sections.iter().map(|(h, _)| h.clone()).collect();
    assert_eq!(
        headings,
        vec![
            Some("Projects".to_string()),
            Some("Projects > Remind Me".to_string()),
            Some("Projects > Remind Me > Rust".to_string()),
        ]
    );
}

#[test]
fn content_before_the_first_heading_has_no_breadcrumb() {
    let sections = split_markdown_sections("preamble\n\n# Heading\n\nbody");
    assert_eq!(sections[0].0, None);
    assert_eq!(sections[0].1, "preamble");
}

#[test]
fn a_heading_inside_a_code_fence_is_not_a_heading() {
    let sections = split_markdown_sections("# Real\n\n```\n# not a heading\n```\n\nbody");
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].0, Some("Real".to_string()));
}

#[test]
fn a_document_chunk_keeps_its_heading_in_the_content() {
    let pairs = parse_document("# Topic\n\nthe body", "md", 10_000);
    assert_eq!(pairs[0].0, "Topic\n\nthe body");
    assert_eq!(pairs[0].1, Some("Topic".to_string()));
}

#[test]
fn a_notes_file_imports_as_a_document() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("notes");
    let path = write(&dir, "notes.md", "# Setup\n\nRun cargo build.");

    let outcome = import(&conn, &path, |_| {});

    match outcome {
        ImportOutcome::Imported { kind, .. } => assert_eq!(kind, ImportKind::Document),
        other => panic!("expected an import, got {:?}", other),
    }
    assert_eq!(column(&conn, "source"), DOCUMENT_SOURCE);
    // The chat-shaped default category gives way for a document.
    assert_eq!(column(&conn, "category"), DOCUMENT_CATEGORY);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_explicit_category_survives_a_document_import() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("cat");
    let path = write(&dir, "notes.md", "# Setup\n\nbody");

    import(&conn, &path, |i| i.category = "runbook".into());

    assert_eq!(column(&conn, "category"), "runbook");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn chat_shaped_markdown_is_detected_as_chat() {
    assert!(looks_like_chat_markdown(
        "## Human\n\nhi\n\n## Assistant\n\nhello"
    ));
    assert!(looks_like_chat_markdown(
        "**User:**\nhi\n**Assistant:**\nhello"
    ));
    assert!(!looks_like_chat_markdown("# Setup\n\nRun cargo build."));
}

#[test]
fn auto_mode_routes_chat_markdown_to_the_chat_parser() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("sniff");
    let path = write(
        &dir,
        "log.md",
        "## Human\n\nwhat is a quokka\n\n## Assistant\n\na small marsupial",
    );

    let outcome = import(&conn, &path, |_| {});

    match outcome {
        ImportOutcome::Imported { kind, .. } => assert_eq!(kind, ImportKind::Chat),
        other => panic!("expected an import, got {:?}", other),
    }
    assert_eq!(contents(&conn), vec!["a small marsupial".to_string()]);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_explicit_kind_overrides_the_sniffer() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("forced");
    let path = write(&dir, "log.md", "## Human\n\nhi\n\n## Assistant\n\nhello");

    import(&conn, &path, |i| i.kind = ImportKind::Document);

    assert_eq!(column(&conn, "source"), DOCUMENT_SOURCE);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_document_import_refuses_json() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("badkind");
    let path = write(&dir, "chat.json", CHAT_JSON);

    let outcome = import(&conn, &path, |i| i.kind = ImportKind::Document);

    assert!(matches!(outcome, ImportOutcome::Failed { .. }));

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- doc_id / chunk_index ----------------------------------------------------

#[test]
fn chunks_share_a_doc_id_and_are_indexed_in_source_order() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("chunks");
    let path = write(
        &dir,
        "notes.md",
        "# One\n\nalpha\n\n# Two\n\nbeta\n\n# Three\n\ngamma",
    );

    import(&conn, &path, |_| {});

    let mut stmt = conn
        .prepare("SELECT doc_id, chunk_index, content FROM memories ORDER BY chunk_index")
        .unwrap();
    let rows: Vec<(String, i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|(doc, _, _)| *doc == rows[0].0));
    assert_eq!(
        rows.iter().map(|(_, i, _)| *i).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(rows[0].2.contains("alpha"));
    assert!(rows[2].2.contains("gamma"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn neighbour_expansion_finally_finds_something() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("neighbours");
    let path = write(
        &dir,
        "notes.md",
        "# One\n\nopening\n\n# Two\n\nquokka section\n\n# Three\n\nclosing",
    );
    import(&conn, &path, |_| {});

    let response = remind_me_core::db::queries::search_with_expansions(
        &conn,
        &remind_me_core::MemorySearchInput {
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
            include_neighbors: true,
            expand_co_retrieval: false,
            bootstrap: false,
        },
    )
    .unwrap();

    // include_neighbors has been shipped and inert since it landed, because
    // only an importer writes doc_id.
    let neighbours = response.related_via_neighbors.unwrap();
    assert_eq!(neighbours.len(), 2, "the sections either side");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_normalize_backlog_finally_has_something_in_it() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("normalize");
    let path = write(&dir, "notes.md", "# Topic\n\nsome raw imported prose");
    import(&conn, &path, |_| {});

    let batch = remind_me_core::normalize::unnormalized_batch(
        &conn,
        &NormalizeBatchInput { batch_size: 20 },
    )
    .unwrap();

    // normalize_batch selects source IN (document_import, chat_import) and has
    // been returning empty since it shipped.
    assert_eq!(batch.total_unnormalized, 1);
    assert_eq!(batch.memories[0].source, DOCUMENT_SOURCE);
    assert_eq!(batch.memories[0].filename.as_deref(), Some("notes.md"));

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- dedup -------------------------------------------------------------------

#[test]
fn re_importing_the_same_file_is_a_no_op() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("dedup");
    let path = write(&dir, "chat.json", CHAT_JSON);

    import(&conn, &path, |_| {});
    let again = import(&conn, &path, |_| {});

    assert!(matches!(again, ImportOutcome::Skipped { .. }));
    assert_eq!(contents(&conn).len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn dedup_is_on_content_not_filename() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("rename");
    let first = write(&dir, "chat.json", CHAT_JSON);
    let renamed = write(&dir, "copy.json", CHAT_JSON);

    import(&conn, &first, |_| {});
    let second = import(&conn, &renamed, |_| {});

    assert!(matches!(second, ImportOutcome::Skipped { .. }));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_edited_file_imports_again() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("edited");
    let path = write(&dir, "notes.md", "# One\n\noriginal");
    import(&conn, &path, |_| {});

    write(&dir, "notes.md", "# One\n\nedited");
    let again = import(&conn, &path, |_| {});

    assert!(matches!(again, ImportOutcome::Imported { .. }));
    assert_eq!(contents(&conn).len(), 2);

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- containment -------------------------------------------------------------

#[test]
fn a_path_outside_the_roots_is_rejected_without_revealing_existence() {
    // Containment runs before the existence check, so a real file and an
    // imaginary one outside the roots must fail identically. Otherwise the
    // importer reports whether arbitrary paths exist.
    assert!(matches!(
        validate_import_file("/etc/passwd"),
        Err(ImportPathError::OutsideRoots(_))
    ));
    assert!(matches!(
        validate_import_file("/etc/definitely-not-here-98765.md"),
        Err(ImportPathError::OutsideRoots(_))
    ));
}

#[test]
fn a_traversal_out_of_the_roots_is_rejected() {
    let home = remind_me_core::import_paths::home_dir_var().unwrap();
    assert!(matches!(
        validate_import_file(&format!("{}/../../etc/passwd", home)),
        Err(ImportPathError::OutsideRoots(_))
    ));
    assert!(matches!(
        validate_import_dir(&format!("{}/../../etc", home)),
        Err(ImportPathError::OutsideRoots(_))
    ));
}

#[test]
fn a_missing_file_inside_the_roots_reports_not_found() {
    let home = remind_me_core::import_paths::home_dir_var().unwrap();
    assert!(matches!(
        validate_import_file(&format!("{}/no_such_file_54321.md", home)),
        Err(ImportPathError::NotFound(_))
    ));
}

#[test]
fn an_unsupported_extension_is_rejected() {
    let dir = scratch("suffix");
    // Not `.png` — images became a supported format when OCR landed, and an
    // example that quietly turns into a supported one stops testing anything.
    let path = write(&dir, "installer.exe", "not really an executable");

    assert!(matches!(
        validate_import_file(&path.display().to_string()),
        Err(ImportPathError::UnsupportedSuffix(_))
    ));

    std::fs::remove_dir_all(&dir).unwrap();
}

// --- directory import --------------------------------------------------------

fn bulk(
    conn: &Connection,
    dir: &std::path::Path,
    recursive: bool,
) -> remind_me_core::BulkImportResult {
    import_directory(
        conn,
        &BulkImportDirInput {
            directory: dir.display().to_string(),
            category: "chat_import".into(),
            tags: vec![],
            extract_mode: "assistant_messages".into(),
            max_length: 10_000,
            recursive,
            kind: ImportKind::Auto,
        },
    )
    .unwrap()
}

#[test]
fn a_directory_import_walks_subdirectories_when_asked() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("bulk");
    write(&dir, "top.md", "# Top\n\nalpha");
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    write(&dir.join("nested"), "deep.md", "# Deep\n\nbeta");

    let result = bulk(&conn, &dir, true);

    assert_eq!(result.files_seen, 2);
    assert_eq!(result.files_imported, 2);
    assert_eq!(result.memories_created, 2);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_non_recursive_import_stays_at_the_top_level() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("flat");
    write(&dir, "top.md", "# Top\n\nalpha");
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    write(&dir.join("nested"), "deep.md", "# Deep\n\nbeta");

    let result = bulk(&conn, &dir, false);

    assert_eq!(result.files_seen, 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unsupported_files_are_passed_over_rather_than_failing_the_run() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("mixed");
    write(&dir, "notes.md", "# Notes\n\nalpha");
    write(&dir, "installer.exe", "binary-ish");

    let result = bulk(&conn, &dir, true);

    // A notes folder holding a stray file of a format nothing here reads
    // should import the markdown beside it, not refuse the lot.
    assert_eq!(result.files_seen, 1);
    assert_eq!(result.files_failed, 0);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_second_directory_run_skips_everything() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("rerun");
    write(&dir, "one.md", "# One\n\nalpha");
    write(&dir, "two.md", "# Two\n\nbeta");
    bulk(&conn, &dir, true);

    let again = bulk(&conn, &dir, true);

    assert_eq!(again.files_skipped, 2);
    assert_eq!(again.files_imported, 0);
    assert_eq!(contents(&conn).len(), 2);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_missing_directory_reports_a_failure_rather_than_erroring() {
    let db = Database::open_in_memory().unwrap();
    let home = remind_me_core::import_paths::home_dir_var().unwrap();

    let result = import_directory(
        &db.conn(),
        &BulkImportDirInput {
            directory: format!("{}/no_such_dir_11111", home),
            category: "chat_import".into(),
            tags: vec![],
            extract_mode: "assistant_messages".into(),
            max_length: 10_000,
            recursive: true,
            kind: ImportKind::Auto,
        },
    )
    .unwrap();

    assert_eq!(result.files_failed, 1);
    assert!(matches!(result.results[0], ImportOutcome::Failed { .. }));
}

// --- graph restore -----------------------------------------------------------

#[test]
fn an_export_restores_its_entity_graph() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("graph");
    // An export carries memories with no record_type, and graph rows with one.
    let path = write(
        &dir,
        "export.json",
        r#"[
          {"role": "assistant", "id": "mem_kept", "content": "about Tasmania"},
          {"record_type": "entity", "id": "ignored", "name": "Tasmania", "kind": "place", "aliases": ["Tas"]},
          {"record_type": "memory_entity", "memory_id": "mem_ghost", "entity_id": "ignored"}
        ]"#,
    );

    let outcome = import(&conn, &path, |_| {});

    match outcome {
        ImportOutcome::Imported { stats, .. } => {
            assert_eq!(stats.entities_restored, 1);
            // The link names a memory id that a re-import did not recreate, so
            // it cannot be restored honestly — it is counted, not invented.
            assert_eq!(stats.links_skipped_dangling, 1);
            assert_eq!(stats.links_restored, 0);
        }
        other => panic!("expected an import, got {:?}", other),
    }

    let entity = remind_me_core::entity::resolve_entity(&conn, "tasmania")
        .unwrap()
        .unwrap();
    assert_eq!(entity.kind.as_deref(), Some("place"));
    assert_eq!(entity.aliases, vec!["Tas".to_string()]);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn graph_records_are_not_imported_as_chat_messages() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let dir = scratch("nomix");
    let path = write(
        &dir,
        "export.json",
        r#"[
          {"role": "assistant", "content": "a real memory"},
          {"record_type": "entity", "name": "Tasmania"}
        ]"#,
    );

    import(&conn, &path, |_| {});

    assert_eq!(contents(&conn), vec!["a real memory".to_string()]);

    std::fs::remove_dir_all(&dir).unwrap();
}
