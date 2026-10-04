//! The write boundary on the engine store: extraction writes
//! `memory_references`, redaction scrubs what is stored and tags it, and
//! attachments are recorded by fingerprint.

use remind_me_core::db::queries;
use remind_me_core::db::references::References;
use remind_me_core::db::Store;
use remind_me_core::{AttachmentInput, Database, EntityInput, Memory, MemoryAddInput};

fn input(content: &str) -> MemoryAddInput {
    MemoryAddInput {
        extract: true,
        attachments: vec![],
        content: content.to_string(),
        category: "general".into(),
        tags: vec![],
        source: "manual".into(),
        metadata: serde_json::json!({}),
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
        sensitive: false,
    }
}

fn add(store: &Store<'_>, input: MemoryAddInput) -> Memory {
    queries::add_memory(store, input).unwrap()
}

fn refs_of(store: &Store<'_>, id: &str) -> Vec<(String, String)> {
    References::new(store)
        .for_memory(id)
        .unwrap()
        .into_iter()
        .map(|r| (r.kind, r.value))
        .collect()
}

#[test]
fn add_writes_extracted_references_and_entities() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let m = add(
        &store,
        input("Fixed rusty_mill/app#321 in 3ac9f797 by editing src/foo.rs:12 for Rusty Mill"),
    );
    let refs = refs_of(&store, &m.id);
    for want in [
        ("issue", "rusty_mill/app#321"),
        ("commit", "3ac9f797"),
        ("path", "src/foo.rs"),
    ] {
        assert!(
            refs.contains(&(want.0.to_string(), want.1.to_string())),
            "{want:?} missing from {refs:?}"
        );
    }
    let hits = References::new(&store)
        .find("issue", "rusty_mill/app#321")
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory_id, m.id);
    assert!(
        remind_me_core::entity::get_entity_by_name(&store, "Rusty Mill")
            .unwrap()
            .is_some()
    );
}

#[test]
fn extract_false_skips_both_passes() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let m = add(
        &store,
        MemoryAddInput {
            extract: false,
            ..input("see o/r#5 about Rusty Mill")
        },
    );
    assert!(refs_of(&store, &m.id).is_empty());
    assert!(
        remind_me_core::entity::get_entity_by_name(&store, "Rusty Mill")
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_caller_entity_keeps_its_kind() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(
        &store,
        MemoryAddInput {
            entities: vec![EntityInput {
                name: "Rusty Mill".into(),
                kind: Some("project".into()),
                aliases: vec![],
            }],
            ..input("Rusty Mill ships weekly")
        },
    );
    let entity = remind_me_core::entity::get_entity_by_name(&store, "Rusty Mill")
        .unwrap()
        .unwrap();
    assert_eq!(entity.kind.as_deref(), Some("project"));
}

#[test]
fn secrets_are_redacted_and_the_row_is_tagged() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let m = add(&store, input("deploy with password=hunter2hunter2 today"));
    assert_eq!(m.content, "deploy with password=[REDACTED:secret] today");
    assert!(m.tags.contains(&"redacted".to_string()), "{:?}", m.tags);
    assert_eq!(m.metadata["redactions"], serde_json::json!(["secret"]));
    let stored = queries::get_memory_by_id(&store, &m.id).unwrap().unwrap();
    assert!(!stored.content.contains("hunter2"));
}

#[test]
fn clean_content_is_not_tagged() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let m = add(&store, input("the token is rotated daily"));
    assert!(!m.tags.contains(&"redacted".to_string()));
    assert!(m.metadata.get("redactions").is_none());
}

#[test]
fn an_attached_file_is_recorded_by_hash_with_its_label() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let dir = std::env::temp_dir().join(format!("rrm_boundary_{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("report.txt");
    std::fs::write(&file, b"hello").unwrap();

    let m = add(
        &store,
        MemoryAddInput {
            attachments: vec![AttachmentInput {
                path: Some(file.to_string_lossy().into_owned()),
                ..Default::default()
            }],
            ..input("see the attached report")
        },
    );
    let rows = References::new(&store).for_memory(&m.id).unwrap();
    let att = rows.iter().find(|r| r.kind == "attachment").unwrap();
    assert_eq!(
        att.value,
        "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    assert_eq!(att.label.as_deref(), Some("report.txt"));
    assert_eq!(m.metadata["attachments"][0]["size"], 5);
    assert_eq!(m.metadata["attachments"][0]["mime"], "text/plain");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_attachment_fails_the_add_and_names_the_path() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let err = queries::add_memory(
        &store,
        MemoryAddInput {
            attachments: vec![AttachmentInput {
                path: Some("/no/such/attachment.bin".into()),
                ..Default::default()
            }],
            ..input("never stored")
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("/no/such/attachment.bin"), "{err}");
}
