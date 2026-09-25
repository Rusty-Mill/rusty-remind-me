//! Coverage for edit history and revert (gap T4, issue #109).
//!
//! The issue's scope warning lists seven mutation paths and asks for the list
//! to be audited against the reference rather than assumed. The audit answer
//! is **one**: the reference records revisions from its update path alone.
//! Tests below pin both halves of that — updates record, the others do not —
//! because "we forgot to wire it up" and "the reference deliberately does not"
//! are indistinguishable from the outside.

use remind_me_core::db::queries;
use remind_me_core::history::{history, revert};
use remind_me_core::{
    Database, MemoryAddInput, MemoryClassification, MemorySearchInput, MemoryUpdateInput,
    ReclassifyInput, RevertOutcome,
};
use rusqlite::Connection;

fn add(conn: &Connection, content: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: "general".into(),
            tags: vec!["original".into()],
            source: "manual".into(),
            metadata: serde_json::json!({"seed": true}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive: false,
        },
    )
    .unwrap()
    .id
}

fn update(conn: &Connection, id: &str, content: Option<&str>, category: Option<&str>) {
    queries::update_memory(
        conn,
        &MemoryUpdateInput {
            memory_id: id.to_string(),
            clear_superseded: false,
            content: content.map(str::to_string),
            category: category.map(str::to_string),
            tags: None,
            metadata: None,
            sensitive: None,
        },
    )
    .unwrap();
}

fn revisions(conn: &Connection, id: &str) -> Vec<remind_me_core::MemoryRevision> {
    history(conn, id, 100).unwrap()
}

// ---------------------------------------------------------------------------
// What records a revision
// ---------------------------------------------------------------------------

#[test]
fn an_update_records_the_value_it_replaced() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "first version");

    update(&conn, &id, Some("second version"), None);

    let revs = revisions(&conn, &id);
    assert_eq!(revs.len(), 1);
    // The snapshot holds the OLD value — a revision that stored the new one
    // would be useless for recovering anything.
    assert_eq!(revs[0].content, "first version");
    assert_eq!(revs[0].category, "general");
    assert!(revs[0].revision_reason.is_none());
}

#[test]
fn each_edit_adds_a_revision_newest_first() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "v1");
    update(&conn, &id, Some("v2"), None);
    update(&conn, &id, Some("v3"), None);

    let revs = revisions(&conn, &id);

    assert_eq!(revs.len(), 2);
    // Ordered by edited_at then id, so a burst of edits within one clock tick
    // still reads in the order they happened.
    assert_eq!(revs[0].content, "v2");
    assert_eq!(revs[1].content, "v1");
}

#[test]
fn a_same_value_update_records_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "unchanged");

    update(&conn, &id, Some("unchanged"), None);

    // Mirrors the outbox trigger's "only on genuine change" discipline. A
    // revision per no-op write would bury the real edits.
    assert!(revisions(&conn, &id).is_empty());
}

#[test]
fn reading_a_memory_records_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokka sighting");

    queries::search_memories(
        &conn,
        &MemorySearchInput {
            query: "quokka".into(),
            ..Default::default()
        },
    )
    .unwrap();

    // Access tracking is an UPDATE against `memories`. If revisions keyed off
    // "any write" rather than the tracked columns, every read would leave one
    // — the same shape of bug issue #100 fixed in the sync outbox.
    assert!(revisions(&conn, &id).is_empty());
}

#[test]
fn reclassifying_records_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "a decision was made");

    queries::reclassify_memories(
        &conn,
        &ReclassifyInput {
            classifications: vec![MemoryClassification {
                memory_id: id.clone(),
                memory_type: "decision".into(),
            }],
        },
    )
    .unwrap();

    // Deliberate, and audited against the reference rather than assumed: it
    // records revisions from the update path alone. Classification is
    // recomputable metadata, and recording it would bury the human edits worth
    // reverting under machine-generated noise.
    assert!(
        revisions(&conn, &id).is_empty(),
        "the reference does not record reclassification; see history.rs's module docs"
    );
}

// ---------------------------------------------------------------------------
// Revert
// ---------------------------------------------------------------------------

#[test]
fn reverting_restores_every_tracked_field_together() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "original text");

    queries::update_memory(
        &conn,
        &MemoryUpdateInput {
            memory_id: id.clone(),
            clear_superseded: false,
            content: Some("rewritten".into()),
            category: Some("engineering".into()),
            tags: Some(vec!["edited".into()]),
            metadata: Some(serde_json::json!({"seed": false})),
            sensitive: Some(true),
        },
    )
    .unwrap();

    let revision_id = revisions(&conn, &id)[0].id;
    let outcome = revert(&conn, &id, revision_id, None).unwrap();

    assert_eq!(outcome, RevertOutcome::Reverted { revision_id });
    let memory = queries::get_memory_by_id(&conn, &id).unwrap().unwrap();
    // All five together — a revert that restored content but left the category
    // and tags from the edit would leave the memory in a state that never
    // existed.
    assert_eq!(memory.content, "original text");
    assert_eq!(memory.category, "general");
    assert_eq!(memory.tags, vec!["original".to_string()]);
    assert_eq!(memory.metadata, serde_json::json!({"seed": true}));
    let sensitive: i64 = conn
        .query_row("SELECT sensitive FROM memories WHERE id = ?", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(sensitive, 0);
}

#[test]
fn a_revert_is_itself_revertable() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "v1");
    update(&conn, &id, Some("v2"), None);

    let first_revision = revisions(&conn, &id)[0].id;
    revert(&conn, &id, first_revision, None).unwrap();
    assert_eq!(
        queries::get_memory_by_id(&conn, &id)
            .unwrap()
            .unwrap()
            .content,
        "v1"
    );

    // The revert recorded the state just before it ran, so undoing it gets v2
    // back. Without that, a mistaken revert would be unrecoverable.
    let revert_revision = revisions(&conn, &id)[0].id;
    assert_ne!(revert_revision, first_revision);
    revert(&conn, &id, revert_revision, None).unwrap();

    assert_eq!(
        queries::get_memory_by_id(&conn, &id)
            .unwrap()
            .unwrap()
            .content,
        "v2"
    );
}

#[test]
fn a_revert_records_why() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "v1");
    update(&conn, &id, Some("v2"), None);
    let revision_id = revisions(&conn, &id)[0].id;

    revert(&conn, &id, revision_id, None).unwrap();

    let reason = revisions(&conn, &id)[0].revision_reason.clone().unwrap();
    assert!(
        reason.contains(&revision_id.to_string()),
        "the default reason should name what was reverted to, got {:?}",
        reason
    );
}

#[test]
fn an_explicit_reason_is_kept() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "v1");
    update(&conn, &id, Some("v2"), None);
    let revision_id = revisions(&conn, &id)[0].id;

    revert(&conn, &id, revision_id, Some("bad edit from the importer")).unwrap();

    assert_eq!(
        revisions(&conn, &id)[0].revision_reason.as_deref(),
        Some("bad edit from the importer")
    );
}

#[test]
fn reverting_to_the_current_state_changes_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "v1");
    update(&conn, &id, Some("v2"), None);
    let revision_id = revisions(&conn, &id)[0].id;
    revert(&conn, &id, revision_id, None).unwrap();
    let before = revisions(&conn, &id).len();

    let outcome = revert(&conn, &id, revision_id, None).unwrap();

    // Reporting it beats writing a no-op revision and an outbox row that says
    // nothing changed.
    assert_eq!(outcome, RevertOutcome::NoChange);
    assert_eq!(revisions(&conn, &id).len(), before);
}

#[test]
fn the_two_not_found_cases_are_distinguished() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "v1");
    update(&conn, &id, Some("v2"), None);
    let revision_id = revisions(&conn, &id)[0].id;

    // They need different fixes — a wrong memory id versus a wrong revision id
    // — so collapsing them into one message would send a caller looking in the
    // wrong place.
    assert_eq!(
        revert(&conn, "mem_nonexistent", revision_id, None).unwrap(),
        RevertOutcome::MemoryNotFound
    );
    assert_eq!(
        revert(&conn, &id, 999_999, None).unwrap(),
        RevertOutcome::RevisionNotFound
    );
}

#[test]
fn a_revision_belonging_to_another_memory_is_refused() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let mine = add(&conn, "mine v1");
    let theirs = add(&conn, "theirs v1");
    update(&conn, &theirs, Some("theirs v2"), None);
    let their_revision = revisions(&conn, &theirs)[0].id;

    let outcome = revert(&conn, &mine, their_revision, None).unwrap();

    // Otherwise one memory's content could be silently pasted over another's.
    assert_eq!(outcome, RevertOutcome::RevisionNotFound);
    assert_eq!(
        queries::get_memory_by_id(&conn, &mine)
            .unwrap()
            .unwrap()
            .content,
        "mine v1"
    );
}

#[test]
fn history_is_scoped_to_one_memory() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = add(&conn, "a v1");
    let b = add(&conn, "b v1");
    update(&conn, &a, Some("a v2"), None);
    update(&conn, &b, Some("b v2"), None);

    assert_eq!(revisions(&conn, &a).len(), 1);
    assert_eq!(revisions(&conn, &a)[0].content, "a v1");
}

#[test]
fn history_respects_its_limit() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "v0");
    for i in 1..=5 {
        update(&conn, &id, Some(&format!("v{}", i)), None);
    }

    assert_eq!(history(&conn, &id, 2).unwrap().len(), 2);
    assert_eq!(history(&conn, &id, 100).unwrap().len(), 5);
}
