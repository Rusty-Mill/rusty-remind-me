//! Coverage for `remind_me_annotate` and the shared entity-mention path.

use remind_me_core::db::queries;
use remind_me_core::entity;
use remind_me_core::{
    AnnotateInput, Database, EntityInput, MemoryAddInput, MemoryAnnotation, ANNOTATE_BATCH_MAX,
};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str) -> String {
    add_with_entities(conn, content, vec![])
}

fn add_with_entities(conn: &Connection, content: &str, entities: Vec<EntityInput>) -> String {
    let input = MemoryAddInput {
        sensitive: false,
        content: content.to_string(),
        category: "general".to_string(),
        tags: vec![],
        source: "manual".to_string(),
        metadata: serde_json::json!({}),
        subject: None,
        predicate: None,
        object: None,
        entities,
    };
    queries::add_memory(conn, input).expect("add failed").id
}

fn annotation(memory_id: &str) -> MemoryAnnotation {
    MemoryAnnotation {
        memory_id: memory_id.to_string(),
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
    }
}

fn ent(name: &str) -> EntityInput {
    EntityInput {
        name: name.to_string(),
        kind: None,
        aliases: vec![],
    }
}

fn linked_entity_ids(conn: &Connection, memory_id: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT entity_id FROM memory_entities WHERE memory_id = ? ORDER BY entity_id")
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![memory_id], |r| r.get::<_, String>(0))
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

#[test]
fn annotate_writes_only_the_supplied_triple_fields() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "a memory");

    queries::annotate_memories(
        &conn,
        &AnnotateInput {
            annotations: vec![MemoryAnnotation {
                predicate: Some("uses".into()),
                ..annotation(&id)
            }],
        },
    )
    .unwrap();

    let m = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();
    assert_eq!(m.predicate.as_deref(), Some("uses"));
    assert_eq!(m.subject, None, "omitted fields stay unset");
    assert_eq!(m.object, None);
}

#[test]
fn annotate_leaves_unmentioned_fields_alone_on_a_second_pass() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "a memory");

    for ann in [
        MemoryAnnotation {
            subject: Some("rusty_remind_me".into()),
            ..annotation(&id)
        },
        MemoryAnnotation {
            object: Some("SQLite".into()),
            ..annotation(&id)
        },
    ] {
        queries::annotate_memories(
            &conn,
            &AnnotateInput {
                annotations: vec![ann],
            },
        )
        .unwrap();
    }

    let m = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();
    assert_eq!(m.subject.as_deref(), Some("rusty_remind_me"));
    assert_eq!(m.object.as_deref(), Some("SQLite"));
}

#[test]
fn annotate_moves_updated_at_but_not_created_at() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "a memory");
    let before = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();

    queries::annotate_memories(
        &conn,
        &AnnotateInput {
            annotations: vec![MemoryAnnotation {
                entities: vec![ent("Tasmania")],
                ..annotation(&id)
            }],
        },
    )
    .unwrap();

    let after = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();
    assert_eq!(after.created_at, before.created_at);
    assert!(
        after.updated_at >= before.updated_at,
        "updated_at moves even for an entities-only annotation"
    );
}

#[test]
fn an_unknown_memory_id_does_not_discard_the_rest_of_the_batch() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let good = add(&conn, "real memory");

    let outcome = queries::annotate_memories(
        &conn,
        &AnnotateInput {
            annotations: vec![
                MemoryAnnotation {
                    subject: Some("ghost".into()),
                    ..annotation("mem_does_not_exist")
                },
                MemoryAnnotation {
                    subject: Some("applied".into()),
                    ..annotation(&good)
                },
            ],
        },
    )
    .unwrap();

    assert_eq!(outcome.errors.len(), 1);
    assert_eq!(outcome.errors[0].memory_id, "mem_does_not_exist");
    assert_eq!(outcome.results.len(), 1);
    assert_eq!(
        queries::get_memory_by_id(&conn, &good)
            .unwrap()
            .unwrap()
            .subject
            .as_deref(),
        Some("applied"),
        "the good annotation must still land"
    );
}

#[test]
fn a_full_batch_applies() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let annotations: Vec<MemoryAnnotation> = (0..ANNOTATE_BATCH_MAX)
        .map(|i| {
            let id = add(&conn, &format!("memory {}", i));
            MemoryAnnotation {
                predicate: Some("uses".into()),
                ..annotation(&id)
            }
        })
        .collect();

    let outcome = queries::annotate_memories(&conn, &AnnotateInput { annotations }).unwrap();
    assert_eq!(outcome.results.len(), ANNOTATE_BATCH_MAX);
    assert!(outcome.errors.is_empty());
}

#[test]
fn entity_mentions_are_linked_and_counted() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "a memory");

    let outcome = queries::annotate_memories(
        &conn,
        &AnnotateInput {
            annotations: vec![MemoryAnnotation {
                entities: vec![ent("Tasmania"), ent("Quokka")],
                ..annotation(&id)
            }],
        },
    )
    .unwrap();

    assert_eq!(outcome.results[0].entities_linked, 2);
    assert_eq!(linked_entity_ids(&conn, &id).len(), 2);
}

#[test]
fn re_mentioning_an_entity_is_idempotent() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "a memory");

    let first = queries::annotate_memories(
        &conn,
        &AnnotateInput {
            annotations: vec![MemoryAnnotation {
                entities: vec![ent("Tasmania")],
                ..annotation(&id)
            }],
        },
    )
    .unwrap();
    let second = queries::annotate_memories(
        &conn,
        &AnnotateInput {
            annotations: vec![MemoryAnnotation {
                entities: vec![ent("Tasmania")],
                ..annotation(&id)
            }],
        },
    )
    .unwrap();

    assert_eq!(first.results[0].entities_linked, 1);
    assert_eq!(
        second.results[0].entities_linked, 0,
        "an existing link counts as zero new links"
    );
    assert_eq!(linked_entity_ids(&conn, &id).len(), 1);
}

#[test]
fn duplicate_entities_within_one_annotation_link_once() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "a memory");

    let outcome = queries::annotate_memories(
        &conn,
        &AnnotateInput {
            annotations: vec![MemoryAnnotation {
                entities: vec![ent("Tasmania"), ent("tasmania"), ent("  Tasmania  ")],
                ..annotation(&id)
            }],
        },
    )
    .unwrap();

    assert_eq!(
        outcome.results[0].entities_linked, 1,
        "casing and whitespace variants are the same entity"
    );
}

#[test]
fn aliases_union_merge_across_mentions() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    entity::upsert_entity(
        &conn,
        &EntityInput {
            name: "Bailey Robertson".into(),
            kind: Some("person".into()),
            aliases: vec!["Bailey".into()],
        },
    )
    .unwrap();

    let merged = entity::upsert_entity(
        &conn,
        &EntityInput {
            name: "Bailey Robertson".into(),
            kind: None,
            aliases: vec!["Bailey".into(), "B.R.".into()],
        },
    )
    .unwrap();

    assert_eq!(
        merged.aliases,
        vec!["Bailey".to_string(), "B.R.".to_string()],
        "existing aliases first, new ones appended, de-duplicated"
    );
}

#[test]
fn an_existing_kind_is_never_clobbered_by_a_later_guess() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    entity::upsert_entity(
        &conn,
        &EntityInput {
            name: "Mercury".into(),
            kind: Some("planet".into()),
            aliases: vec![],
        },
    )
    .unwrap();

    let after = entity::upsert_entity(
        &conn,
        &EntityInput {
            name: "Mercury".into(),
            kind: Some("element".into()),
            aliases: vec![],
        },
    )
    .unwrap();

    assert_eq!(
        after.kind.as_deref(),
        Some("planet"),
        "the deliberate earlier kind wins over a later mention"
    );
}

#[test]
fn a_missing_kind_is_filled_in_by_a_later_mention() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    entity::upsert_entity(&conn, &ent("Quokka")).unwrap();
    let after = entity::upsert_entity(
        &conn,
        &EntityInput {
            name: "Quokka".into(),
            kind: Some("animal".into()),
            aliases: vec![],
        },
    )
    .unwrap();

    assert_eq!(after.kind.as_deref(), Some("animal"));
}

#[test]
fn add_memory_applies_its_entities_instead_of_dropping_them() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // MemoryAddInput has always accepted `entities`; it used to parse and
    // discard them, so a caller supplying mentions got a silent no-op.
    let id = add_with_entities(&conn, "mentions a place", vec![ent("Tasmania")]);

    assert_eq!(
        linked_entity_ids(&conn, &id).len(),
        1,
        "entities supplied to add_memory must be linked"
    );
    assert!(entity::get_entity_by_name(&conn, "Tasmania")
        .unwrap()
        .is_some());
}

#[test]
fn deleting_an_annotated_memory_drops_its_links_but_keeps_the_entity() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let doomed = add_with_entities(&conn, "going away", vec![ent("Tasmania")]);
    let keeper = add_with_entities(&conn, "staying", vec![ent("Tasmania")]);

    queries::delete_memory(&conn, &doomed).unwrap();

    assert!(linked_entity_ids(&conn, &doomed).is_empty());
    assert_eq!(
        linked_entity_ids(&conn, &keeper).len(),
        1,
        "the other memory keeps its link"
    );
    assert!(
        entity::get_entity_by_name(&conn, "Tasmania")
            .unwrap()
            .is_some(),
        "the entity survives; other memories may still cite it"
    );
}

#[test]
fn blank_entity_names_are_skipped() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "a memory");

    let outcome = queries::annotate_memories(
        &conn,
        &AnnotateInput {
            annotations: vec![MemoryAnnotation {
                entities: vec![ent("   "), ent("Real")],
                ..annotation(&id)
            }],
        },
    )
    .unwrap();

    assert_eq!(outcome.results[0].entities_linked, 1);
}
