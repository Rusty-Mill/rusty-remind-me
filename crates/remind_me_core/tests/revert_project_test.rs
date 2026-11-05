//! A project edit is revertible (Mill Board card #702).
//!
//! `remind_me_update` can set `project` since #359, but the edit made no
//! revision, so `remind_me_revert` could not undo it. Revisions now snapshot
//! `project` (schema v33), and a revert restores it, back to `null` when that is what the revision holds.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::context::CWD_ENV;
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::history::{history, revert};
use remind_me_core::{Database, MemoryAddInput, MemoryUpdateInput, RevertOutcome};

/// A memory with no project: written from `/`, whose name derives none, as
/// a connector write was before #358.
fn add(store: &Store<'_>, content: &str) -> String {
    crate::test_env::set_var(CWD_ENV, "/");
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.to_string(),
            category: "general".into(),
            source: "manual".into(),
            metadata: serde_json::json!({}),
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

fn edit(store: &Store<'_>, id: &str, content: Option<&str>, project: Option<&str>) {
    queries::update_memory(
        store,
        &MemoryUpdateInput {
            memory_id: id.to_string(),
            content: content.map(str::to_string),
            project: project.map(str::to_string),
            category: None,
            tags: None,
            metadata: None,
            sensitive: None,
            clear_superseded: false,
        },
    )
    .unwrap();
}

fn project_of(store: &Store<'_>, id: &str) -> Option<String> {
    queries::get_memory_by_id(store, id)
        .unwrap()
        .unwrap()
        .project
}

fn newest_revision(store: &Store<'_>, id: &str) -> i64 {
    history(store, id, 1).unwrap()[0].id
}

#[test]
fn reverting_a_project_edit_restores_null() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "note");
    assert_eq!(project_of(&store, &id), None);

    edit(&store, &id, None, Some("let-em-loose"));
    assert_eq!(project_of(&store, &id).as_deref(), Some("let-em-loose"));

    let outcome = revert(&store, &id, newest_revision(&store, &id), None).unwrap();

    assert!(
        matches!(outcome, RevertOutcome::Reverted { .. }),
        "{outcome:?}"
    );
    assert_eq!(project_of(&store, &id), None, "back to no project");
}

#[test]
fn reverting_restores_an_earlier_project() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "note");
    edit(&store, &id, None, Some("alpha"));
    edit(&store, &id, None, Some("beta"));

    let to_alpha = newest_revision(&store, &id);
    assert_eq!(
        history(&store, &id, 1).unwrap()[0].project,
        Some(Some("alpha".to_string()))
    );
    revert(&store, &id, to_alpha, None).unwrap();

    assert_eq!(project_of(&store, &id).as_deref(), Some("alpha"));
}

#[test]
fn a_project_only_edit_makes_exactly_one_revision() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "note");

    edit(&store, &id, None, Some("alpha"));
    assert_eq!(history(&store, &id, 10).unwrap().len(), 1);

    edit(&store, &id, None, Some("alpha"));
    assert_eq!(
        history(&store, &id, 10).unwrap().len(),
        1,
        "setting the same project again is no change"
    );
}

#[test]
fn an_edit_that_omits_the_project_leaves_it_alone() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let id = add(&store, "first");
    edit(&store, &id, None, Some("alpha"));

    edit(&store, &id, Some("second"), None);
    assert_eq!(project_of(&store, &id).as_deref(), Some("alpha"));

    // Reverting the content edit restores the content and keeps the project
    // it held then, which is the same one.
    revert(&store, &id, newest_revision(&store, &id), None).unwrap();
    let memory = queries::get_memory_by_id(&store, &id).unwrap().unwrap();
    assert_eq!(memory.content, "first");
    assert_eq!(memory.project.as_deref(), Some("alpha"));
}
