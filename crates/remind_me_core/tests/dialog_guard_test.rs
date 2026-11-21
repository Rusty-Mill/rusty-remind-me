//! Dialog rows are the verbatim record (#1616, D13): every generic write path
//! refuses to edit or delete one, consolidation and re-import leave them
//! alone, they do not decay or count toward vault health, and default search
//! leaves them out unless asked.

use remind_me_core::consolidation::consolidate;
use remind_me_core::db::memories::Memories;
use remind_me_core::db::queries;
use remind_me_core::db::{Store, StoreError};
use remind_me_core::importer::import_chat;
use remind_me_core::models::{AnnotateInput, MemoryAnnotation, DIALOG_CATEGORY};
use remind_me_core::undo_import::undo_import;
use remind_me_core::vitality::{build_vitality_report, never_dormant};
use remind_me_core::{
    BulkTagInput, ChatImportInput, ConsolidateInput, Database, ImportKind, ImportOutcome,
    MemoryAddInput, MemoryClassification, MemorySearchInput, MemoryUpdateInput,
    ReclassifyBatchInput, ReclassifyInput, TagMode, UndoImportInput, UndoImportKind,
};
use serde_json::json;

fn add(store: &Store<'_>, content: &str, category: &str, metadata: serde_json::Value) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.to_string(),
            category: category.to_string(),
            source: "manual".into(),
            metadata,
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

fn dialog(store: &Store<'_>, content: &str) -> String {
    add(store, content, DIALOG_CATEGORY, json!({}))
}

fn content(store: &Store<'_>, id: &str) -> String {
    queries::get_memory_by_id(store, id)
        .unwrap()
        .unwrap()
        .content
}

fn assert_refused(result: Result<impl std::fmt::Debug, StoreError>, id: &str) {
    let refused = result.unwrap_err().to_string();
    assert!(refused.contains(&format!("{id} is a dialog")), "{refused}");
}

fn update(id: &str, fields: serde_json::Value) -> MemoryUpdateInput {
    let mut input = json!({ "memory_id": id });
    input
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    serde_json::from_value(input).unwrap()
}

#[test]
fn update_of_a_dialog_is_refused_and_leaves_it_as_it_was() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = dialog(&store, "Nano: keep everything");

    for fields in [
        json!({ "content": "rewritten" }),
        json!({ "category": "fact" }),
        json!({ "tags": ["x"] }),
        json!({ "sensitive": true }),
    ] {
        assert_refused(queries::update_memory(&store, &update(&id, fields)), &id);
    }
    let after = queries::get_memory_by_id(&store, &id).unwrap().unwrap();
    assert_eq!(after.content, "Nano: keep everything");
    assert_eq!(after.category, DIALOG_CATEGORY);
    assert!(after.tags.is_empty() && !after.sensitive);
}

#[test]
fn updating_a_memory_into_a_dialog_is_allowed() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(
        &store,
        "a mistyped transcript window",
        "learning",
        json!({}),
    );

    queries::update_memory(&store, &update(&id, json!({ "category": DIALOG_CATEGORY }))).unwrap();
    // From then on it is protected.
    assert_refused(
        queries::update_memory(&store, &update(&id, json!({ "category": "learning" }))),
        &id,
    );
}

#[test]
fn delete_of_a_dialog_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = dialog(&store, "Nano: hello");

    assert_refused(queries::delete_memory(&store, &id), &id);
    assert_eq!(content(&store, &id), "Nano: hello");
}

#[test]
fn bulk_delete_with_a_dialog_in_it_refuses_the_whole_batch() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let fact = add(&store, "a fact", "fact", json!({}));
    let id = dialog(&store, "Nano: hello");

    assert_refused(
        queries::bulk_delete(&store, &[fact.clone(), id.clone()]),
        &id,
    );
    assert!(queries::get_memory_by_id(&store, &fact).unwrap().is_some());
    assert!(queries::get_memory_by_id(&store, &id).unwrap().is_some());
}

#[test]
fn bulk_tag_with_a_dialog_in_it_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = dialog(&store, "Nano: hello");
    let input = BulkTagInput {
        ids: vec![id.clone()],
        tags: vec!["junk".into()],
        mode: TagMode::Add,
    };

    assert_refused(queries::bulk_tag(&store, &input), &id);
}

#[test]
fn annotating_a_dialog_reports_a_refusal_for_that_item() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = dialog(&store, "Nano: hello");
    let input = AnnotateInput {
        annotations: vec![MemoryAnnotation {
            memory_id: id.clone(),
            subject: Some("Nano".into()),
            predicate: Some("said".into()),
            object: Some("hello".into()),
            entities: vec![],
        }],
    };

    let result = queries::annotate_memories(&store, &input).unwrap();
    assert!(result.errors[0].error.contains("is a dialog"), "{result:?}");
    let after = queries::get_memory_by_id(&store, &id).unwrap().unwrap();
    assert!(after.subject.is_none() && after.predicate.is_none());
}

#[test]
fn reclassify_leaves_a_dialog_alone_and_says_so() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = dialog(&store, "Nano: hello");
    let fact = add(&store, "a plain note", "general", json!({}));

    let result = queries::reclassify_memories(
        &store,
        &ReclassifyInput {
            classifications: [&id, &fact]
                .map(|m| MemoryClassification {
                    memory_id: m.clone(),
                    memory_type: "fact".into(),
                })
                .to_vec(),
        },
    )
    .unwrap();

    assert_eq!(result.updated, 1);
    assert_eq!(result.refused, vec![id.clone()]);
    let after = queries::get_memory_by_id(&store, &id).unwrap().unwrap();
    assert_ne!(after.memory_type.as_deref(), Some("fact"));
}

#[test]
fn the_unclassified_batch_does_not_offer_dialogs() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = dialog(&store, "Nano: hello");
    let note = add(&store, "a plain note", "general", json!({}));

    let batch =
        queries::unclassified_batch(&store, &ReclassifyBatchInput { batch_size: 50 }).unwrap();
    let offered: Vec<&str> = batch.memories.iter().map(|m| m.id.as_str()).collect();
    assert!(offered.contains(&note.as_str()), "{offered:?}");
    assert!(!offered.contains(&id.as_str()), "{offered:?}");
}

#[test]
fn naming_a_dialog_in_supersedes_refuses_the_add() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = dialog(&store, "Nano: hello");

    let refused = queries::add_memory_with(
        &store,
        MemoryAddInput {
            content: "a cleaner version".into(),
            supersedes: vec![id.clone()],
            ..Default::default()
        },
        &Default::default(),
    );
    assert_refused(refused, &id);
    let after = queries::get_memory_by_id(&store, &id).unwrap().unwrap();
    assert!(after.superseded_by.is_none());
}

#[test]
fn consolidation_never_offers_a_dialog_for_merging() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let vectors = remind_me_core::db::vectors::Vectors::new(&store);
    let bytes: Vec<u8> = [1.0f32, 0.0].iter().flat_map(|f| f.to_le_bytes()).collect();
    for i in 0..2 {
        let id = dialog(&store, &format!("Nano: the same words {i}"));
        vectors.put(&id, 0, &bytes).unwrap();
    }

    let report = consolidate(
        &store,
        &ConsolidateInput {
            similarity_threshold: 0.85,
            dry_run: true,
            category: None,
            limit: 500,
            summaries: None,
        },
    )
    .unwrap();
    assert_eq!(report["clusters_found"], 0, "{report}");
}

#[test]
fn a_reimport_supersedes_the_old_chunks_but_not_its_dialogs() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let chunk = add(
        &store,
        "a doc chunk",
        "general",
        json!({ "import_id": "imp_old" }),
    );
    let id = add(
        &store,
        "Nano: hi",
        DIALOG_CATEGORY,
        json!({ "import_id": "imp_old" }),
    );

    let retired = remind_me_core::watcher::supersede_import(&store, "imp_old", "imp_new").unwrap();

    assert_eq!(retired, 1);
    let superseded = |id: &str| {
        Memories::new(&store)
            .get_many(&[id.to_string()])
            .unwrap()
            .remove(0)
            .superseded_by
    };
    assert_eq!(superseded(&chunk).as_deref(), Some("imp_new"));
    assert_eq!(superseded(&id), None);
}

#[test]
fn undoing_an_import_that_holds_dialogs_removes_nothing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // An import source has to sit inside the import roots (the home directory).
    let dir = std::path::PathBuf::from(remind_me_core::import_paths::home_dir_var().unwrap())
        .join(format!("rrm_dialog_guard_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("chat.json");
    std::fs::write(
        &path,
        r#"[{"role":"user","content":"keep this"},{"role":"assistant","content":"kept"}]"#,
    )
    .unwrap();
    let imported = import_chat(
        &store,
        &ChatImportInput {
            file_path: path.display().to_string(),
            category: DIALOG_CATEGORY.into(),
            tags: vec![],
            extract_mode: "all".into(),
            max_length: 10_000,
            kind: ImportKind::Auto,
        },
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let ImportOutcome::Imported { import_id, .. } = imported else {
        panic!("expected an import, got {imported:?}");
    };

    for dry_run in [true, false] {
        let refused = undo_import(
            &store,
            &UndoImportInput {
                import_kind: UndoImportKind::Chat,
                import_id: Some(import_id.clone()),
                dry_run,
                limit: 100,
            },
        );
        let message = refused.unwrap_err().to_string();
        assert!(message.contains("dialog rows"), "{message}");
    }
}

#[test]
fn dialogs_never_go_dormant_and_stay_out_of_vault_health() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = dialog(&store, "Nano: hello");
    add(&store, "a fact", "fact", json!({}));

    let memory = queries::get_memory_by_id(&store, &id).unwrap().unwrap();
    assert!(never_dormant(&memory));
    let report = build_vitality_report(&store).unwrap();
    assert_eq!(report.total_memories, 1);
    assert!(!report.decay_distribution.contains_key(DIALOG_CATEGORY));
}

#[test]
fn default_search_leaves_dialogs_out_and_include_raw_brings_them_back() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = dialog(&store, "Nano: the zanzibar plan");
    let fact = add(&store, "the zanzibar plan is approved", "fact", json!({}));
    let search = |include_raw: bool| -> Vec<String> {
        queries::search_memories(
            &store,
            &MemorySearchInput {
                query: "zanzibar".into(),
                include_raw,
                ..Default::default()
            },
        )
        .unwrap()
        .into_iter()
        .map(|r| r.memory.id)
        .collect()
    };

    let default = search(false);
    assert!(
        default.contains(&fact) && !default.contains(&id),
        "{default:?}"
    );
    assert!(search(true).contains(&id));
}

fn add_triple(store: &Store<'_>, category: &str, object: &str) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: format!("the store is {object}"),
            category: category.to_string(),
            source: "manual".into(),
            subject: Some("remind-me store".into()),
            predicate: Some("is".into()),
            object: Some(object.into()),
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

#[test]
fn a_contradicting_fact_supersedes_a_fact_but_never_a_dialog() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let said = add_triple(&store, DIALOG_CATEGORY, "sqlite");
    let fact = add_triple(&store, "fact", "sqlite");

    let newer = add_triple(&store, "fact", "attest");
    // What a capture's facts and an annotation run after writing a triple.
    let superseded = remind_me_core::entity::supersede_contradicting_facts(
        &store,
        &newer,
        Some("remind-me store"),
        Some("is"),
        Some("attest"),
    )
    .unwrap();
    assert_eq!(superseded, vec![fact.clone()]);

    let superseded_by = |id: &str| {
        Memories::new(&store)
            .get_many(&[id.to_string()])
            .unwrap()
            .remove(0)
            .superseded_by
    };
    assert_eq!(superseded_by(&fact).as_deref(), Some(newer.as_str()));
    assert_eq!(superseded_by(&said), None);
    assert!(queries::get_memory_by_id(&store, &said).unwrap().is_some());
}

fn session_line(role: &str, text: &str) -> String {
    json!({
        "type": role, "sessionId": "s-frozen", "uuid": format!("u-{text}"),
        "timestamp": "2026-11-21T10:00:00.000Z",
        "message": {"role": role, "content": text}
    })
    .to_string()
}

#[test]
fn recapturing_a_captured_session_leaves_the_dialog_row_unchanged() {
    use remind_me_core::transcript::{capture_transcript, TranscriptRequest, CONTINUATION_TYPE};
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let request = |reason| TranscriptRequest {
        session_id: "s-frozen",
        cwd: None,
        reason: Some(reason),
        context: None,
    };
    let first = [
        session_line("user", "first ask"),
        session_line("assistant", "first reply"),
    ];
    let made = capture_transcript(&store, &first.join("\n"), &request("stop")).unwrap();
    let before = queries::get_memory_by_id(&store, &made.dialog_id)
        .unwrap()
        .unwrap();

    let grown = [first.join("\n"), session_line("user", "a later ask")].join("\n");
    let again = capture_transcript(&store, &grown, &request("end")).unwrap();

    let after = queries::get_memory_by_id(&store, &made.dialog_id)
        .unwrap()
        .unwrap();
    assert_eq!(again.dialog_id, made.dialog_id);
    assert_eq!(after.content, before.content);
    assert_eq!(after.metadata, before.metadata);
    assert_eq!(after.tags, before.tags);
    assert_eq!(after.updated_at, before.updated_at);
    let history = remind_me_core::db::history::Revisions::new(&store)
        .list(&made.dialog_id, 10)
        .unwrap();
    assert!(history.is_empty(), "{history:?}");

    // The later turn is kept, in its own read-only dialog row.
    let continuation = remind_me_core::episodes::session_memories(&store, "s-frozen")
        .unwrap()
        .into_iter()
        .find(|m| m.metadata["type"] == CONTINUATION_TYPE)
        .unwrap();
    assert!(continuation.content.contains("a later ask"));
    assert!(!continuation.content.contains("first ask"));
    assert_eq!(continuation.category, DIALOG_CATEGORY);
    assert_refused(
        queries::delete_memory(&store, &continuation.id),
        &continuation.id,
    );
}

fn frozen_request(reason: &str) -> remind_me_core::transcript::TranscriptRequest<'_> {
    remind_me_core::transcript::TranscriptRequest {
        session_id: "s-frozen",
        cwd: None,
        reason: Some(reason),
        context: None,
    }
}

fn turns(range: std::ops::RangeInclusive<usize>) -> String {
    range
        .map(|i| {
            session_line(
                if i % 2 == 1 { "user" } else { "assistant" },
                &format!("turn {i}"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn continuations(store: &Store<'_>) -> Vec<remind_me_core::models::Memory> {
    let mut rows: Vec<_> = remind_me_core::episodes::session_memories(store, "s-frozen")
        .unwrap()
        .into_iter()
        .filter(|m| m.metadata["type"] == remind_me_core::transcript::CONTINUATION_TYPE)
        .collect();
    rows.sort_by_key(|m| m.metadata[remind_me_core::models::SEQUENCE_KEY].as_u64());
    rows
}

#[test]
fn a_tail_sent_mid_session_records_only_the_turns_after_the_overlap() {
    use remind_me_core::transcript::{capture_transcript, captured_messages};
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    capture_transcript(&store, &turns(1..=5), &frozen_request("stop")).unwrap();

    // Over the size limit, a caller sends the tail: turns 3 to 7.
    let made = capture_transcript(&store, &turns(3..=7), &frozen_request("stop")).unwrap();

    let added = continuations(&store);
    assert_eq!(added.len(), 1);
    let text = &added[0].content;
    assert!(text.contains("turn 6") && text.contains("turn 7"), "{text}");
    assert!(
        !text.contains("turn 3") && !text.contains("turn 5"),
        "{text}"
    );
    assert_eq!(added[0].metadata["from_message"], json!(5));
    assert_eq!(added[0].metadata["messages"], json!(2));
    assert_eq!(added[0].metadata["placement"], json!("id"));
    // The count comes from the dialog rows: 5 + 2.
    assert_eq!(made.messages, 7);
    assert_eq!(captured_messages(&store, "s-frozen").unwrap(), Some(7));

    // The same tail again adds nothing.
    capture_transcript(&store, &turns(3..=7), &frozen_request("stop")).unwrap();
    assert_eq!(continuations(&store).len(), 1);
}

#[test]
fn the_recorded_count_ignores_what_the_summary_claims() {
    use remind_me_core::transcript::{capture_transcript, captured_messages};
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let made = capture_transcript(&store, &turns(1..=3), &frozen_request("stop")).unwrap();
    capture_transcript(&store, &turns(1..=4), &frozen_request("stop")).unwrap();
    // A summary that overstates the count must not hide new turns.
    let summary = queries::get_memory_by_id(&store, &made.summary_id)
        .unwrap()
        .unwrap();
    let mut metadata = summary.metadata.clone();
    metadata["messages"] = json!(99);
    Memories::new(&store)
        .set_tags_and_metadata(&made.summary_id, &summary.tags, &metadata)
        .unwrap();

    assert_eq!(captured_messages(&store, "s-frozen").unwrap(), Some(4));
    capture_transcript(&store, &turns(1..=5), &frozen_request("stop")).unwrap();
    assert!(continuations(&store)
        .iter()
        .any(|m| m.content.contains("turn 5")));
}

/// A transcript line with no `uuid`, so placement can only use text.
fn bare_line(role: &str, text: &str) -> String {
    json!({
        "type": role, "sessionId": "s-frozen",
        "timestamp": "2026-11-21T10:00:00.000Z",
        "message": {"role": role, "content": text}
    })
    .to_string()
}

fn bare(texts: &[&str]) -> String {
    texts
        .iter()
        .enumerate()
        .map(|(i, t)| bare_line(if i % 2 == 0 { "user" } else { "assistant" }, t))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_resent_older_copy_adds_nothing_by_id_or_by_text() {
    use remind_me_core::transcript::{capture_transcript, captured_messages};
    for (first, grown, older) in [
        (turns(1..=4), turns(1..=6), turns(1..=3)),
        (
            bare(&["a1", "b1", "a2", "b2"]),
            bare(&["a1", "b1", "a2", "b2", "a3", "b3"]),
            bare(&["a1", "b1", "a2"]),
        ),
    ] {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let made = capture_transcript(&store, &first, &frozen_request("stop")).unwrap();
        capture_transcript(&store, &grown, &frozen_request("stop")).unwrap();
        let summary = queries::get_memory_by_id(&store, &made.summary_id)
            .unwrap()
            .unwrap();

        // The older, shorter copy arrives late.
        let late = capture_transcript(&store, &older, &frozen_request("stop")).unwrap();

        assert_eq!(
            continuations(&store).len(),
            1,
            "only the grown copy added rows"
        );
        assert_eq!(late.messages, 6);
        assert_eq!(captured_messages(&store, "s-frozen").unwrap(), Some(6));
        let after = queries::get_memory_by_id(&store, &made.summary_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            after.content, summary.content,
            "the summary still describes 6"
        );
    }
}

#[test]
fn repeated_short_turns_by_text_are_kept_whole_and_flagged_ambiguous() {
    use remind_me_core::transcript::capture_transcript;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    capture_transcript(
        &store,
        &bare(&["q1", "yes", "q2", "yes"]),
        &frozen_request("stop"),
    )
    .unwrap();

    // A tail that starts at "yes": which "yes" it is cannot be shown.
    let tail = [bare_line("assistant", "yes"), bare_line("user", "q3")].join("\n");
    capture_transcript(&store, &tail, &frozen_request("stop")).unwrap();

    let added = continuations(&store);
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].metadata["placement"], json!("unplaced_ambiguous"));
    assert_eq!(
        added[0].metadata["messages"],
        json!(2),
        "kept whole, not guessed"
    );
    assert!(added[0].content.contains("yes") && added[0].content.contains("q3"));
}

#[test]
fn repeated_short_turns_with_ids_are_placed_exactly() {
    use remind_me_core::transcript::capture_transcript;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let line = |i: usize, role: &str, text: &str| {
        json!({
            "type": role, "sessionId": "s-frozen", "uuid": format!("u{i}"),
            "timestamp": "2026-11-21T10:00:00.000Z",
            "message": {"role": role, "content": text}
        })
        .to_string()
    };
    let session = [
        line(1, "user", "q1"),
        line(2, "assistant", "yes"),
        line(3, "user", "q2"),
        line(4, "assistant", "yes"),
        line(5, "user", "q3"),
        line(6, "assistant", "yes"),
    ];
    capture_transcript(&store, &session[..4].join("\n"), &frozen_request("stop")).unwrap();

    capture_transcript(&store, &session[3..].join("\n"), &frozen_request("stop")).unwrap();

    let added = continuations(&store);
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].metadata["placement"], json!("id"));
    assert_eq!(added[0].metadata["message_ids"], json!(["u5", "u6"]));
    assert_eq!(added[0].metadata["from_message"], json!(4));
}

#[test]
fn a_text_transcript_with_no_recorded_turn_is_kept_whole_and_flagged() {
    use remind_me_core::transcript::capture_transcript;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    capture_transcript(&store, &bare(&["a1", "b1", "a2"]), &frozen_request("stop")).unwrap();

    capture_transcript(&store, &bare(&["x1", "y1"]), &frozen_request("stop")).unwrap();

    let added = continuations(&store);
    assert_eq!(added.len(), 1);
    assert!(added[0].content.contains("x1") && added[0].content.contains("y1"));
    assert_eq!(added[0].metadata["placement"], json!("unplaced_no_overlap"));
}

#[test]
fn reverting_a_dialog_is_refused_and_writes_no_revision() {
    use remind_me_core::db::history::Revisions;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // A row edited before it became a dialog has revisions to revert to.
    let id = add(&store, "first words", "learning", json!({}));
    queries::update_memory(&store, &update(&id, json!({ "content": "second words" }))).unwrap();
    queries::update_memory(&store, &update(&id, json!({ "category": DIALOG_CATEGORY }))).unwrap();
    let history = Revisions::new(&store).list(&id, 10).unwrap();
    let oldest = history.last().unwrap().id;
    let before = queries::get_memory_by_id(&store, &id).unwrap().unwrap();

    let refused = remind_me_core::history::revert(&store, &id, oldest, None);

    assert_refused(refused, &id);
    let after = queries::get_memory_by_id(&store, &id).unwrap().unwrap();
    assert_eq!(after.content, before.content);
    assert_eq!(after.category, DIALOG_CATEGORY);
    assert_eq!(after.updated_at, before.updated_at);
    assert_eq!(
        Revisions::new(&store).list(&id, 10).unwrap().len(),
        history.len()
    );
}

#[test]
fn a_new_turn_that_starts_an_earlier_one_is_kept_not_held() {
    use remind_me_core::transcript::{capture_transcript, captured_messages};
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    capture_transcript(&store, &bare(&["a1", "do it now"]), &frozen_request("stop")).unwrap();

    // "do it" is only the start of the recorded "do it now": a new turn.
    capture_transcript(&store, &bare(&["a1", "do it"]), &frozen_request("stop")).unwrap();

    let rows = continuations(&store);
    assert_eq!(rows.len(), 1, "kept, not taken as held");
    assert_eq!(rows[0].metadata["placement"], "unplaced_no_overlap");
    assert!(rows[0].content.ends_with("do it"), "{}", rows[0].content);
    assert_eq!(captured_messages(&store, "s-frozen").unwrap(), Some(4));
}

#[test]
fn a_recaptured_sensitive_dialog_keeps_its_new_rows_sensitive() {
    use remind_me_core::db::memories::MemoryEdit;
    use remind_me_core::transcript::capture_transcript;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let made = capture_transcript(&store, &turns(1..=2), &frozen_request("stop")).unwrap();
    // Marked sensitive below the guarded paths, as a capture of a sensitive
    // session would be.
    Memories::new(&store)
        .apply_edit(
            &made.dialog_id,
            &MemoryEdit {
                sensitive: Some(true),
                ..MemoryEdit::at("2026-11-21T10:00:01Z")
            },
        )
        .unwrap();

    capture_transcript(&store, &turns(1..=4), &frozen_request("stop")).unwrap();

    let rows = continuations(&store);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].sensitive, "the new row stays sensitive");
}

#[test]
fn unplaced_rows_do_not_make_the_watcher_skip_a_longer_reading() {
    use remind_me_core::transcript::{
        capture_transcript, capture_unless_held, Conditional, Replace,
    };
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    capture_transcript(&store, &bare(&["a1", "b1"]), &frozen_request("stop")).unwrap();
    // Unplaced: two more held messages that may repeat nothing or anything.
    capture_transcript(&store, &bare(&["x1", "y1"]), &frozen_request("stop")).unwrap();

    // Three messages beat the two placed ones, not the four held in all.
    let watched = capture_unless_held(
        &store,
        &bare(&["a1", "b1", "a2"]),
        &frozen_request("watch"),
        Replace::IfLonger,
    )
    .unwrap();
    assert!(matches!(watched, Conditional::Refreshed(_)), "{watched:?}");
    let rows = continuations(&store);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].metadata["placement"], "overlap");
    assert!(rows[1].content.ends_with("a2"), "{}", rows[1].content);
}

#[test]
fn an_unplaced_sensitive_continuation_is_as_visible_as_its_first_row() {
    use remind_me_core::db::memories::MemoryEdit;
    use remind_me_core::transcript::capture_transcript;
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let made = capture_transcript(
        &store,
        &bare(&["the quokka plan", "noted"]),
        &frozen_request("stop"),
    )
    .unwrap();
    Memories::new(&store)
        .apply_edit(
            &made.dialog_id,
            &MemoryEdit {
                sensitive: Some(true),
                ..MemoryEdit::at("2026-11-21T10:00:01Z")
            },
        )
        .unwrap();
    capture_transcript(
        &store,
        &bare(&["another quokka thread", "fine"]),
        &frozen_request("stop"),
    )
    .unwrap();
    let rows = continuations(&store);
    assert_eq!(rows.len(), 1);
    let continuation = &rows[0];
    assert_eq!(continuation.metadata["placement"], "unplaced_no_overlap");
    assert!(continuation.sensitive);

    // Single reads: by id both are there; search shows both only when raw
    // and sensitive rows are asked for, and neither otherwise.
    for id in [&made.dialog_id, &continuation.id] {
        let row = queries::get_memory_by_id(&store, id).unwrap().unwrap();
        assert!(row.sensitive, "{id}");
    }
    let search = |include_raw: bool, include_sensitive: bool| -> Vec<String> {
        queries::search_memories(
            &store,
            &MemorySearchInput {
                query: "quokka".into(),
                include_raw,
                include_sensitive,
                ..Default::default()
            },
        )
        .unwrap()
        .into_iter()
        .map(|r| r.memory.id)
        .collect()
    };
    for (raw, sensitive) in [(false, false), (true, false), (false, true)] {
        let found = search(raw, sensitive);
        assert!(
            !found.contains(&made.dialog_id) && !found.contains(&continuation.id),
            "raw={raw} sensitive={sensitive}: {found:?}"
        );
    }
    let found = search(true, true);
    assert!(found.contains(&made.dialog_id), "{found:?}");
    assert!(found.contains(&continuation.id), "{found:?}");

    // Joined reads: the capture holds both rows, each still marked, and the
    // whole dialog joins them in sequence.
    let capture = remind_me_core::capture::get_capture(&store, &made.capture_id)
        .unwrap()
        .unwrap();
    assert!(capture.dialog.as_ref().unwrap().sensitive);
    let joined = capture.continuations();
    assert_eq!(joined.len(), 1);
    assert!(joined[0].sensitive);
    let full = capture.full_dialog().unwrap();
    let first_at = full.find("the quokka plan").unwrap();
    let later_at = full.find("another quokka thread").unwrap();
    assert!(first_at < later_at, "{full}");
}
