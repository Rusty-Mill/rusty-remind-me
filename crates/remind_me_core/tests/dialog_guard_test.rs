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
