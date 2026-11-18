//! The decisions-log export (card #1565): which decisions it takes, in what
//! order, and that re-running it gives the same bytes.
use remind_me_core::daemon::ops::{execute, Op};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::Database;
use serde_json::json;

fn decision(id: &str, content: &str, created: &str) -> NewMemory {
    let mut row = NewMemory::new(id, content, created);
    row.memory_type = "decision".into();
    row.project = Some("mill".into());
    row
}

fn export(db: &Database, include_superseded: bool) -> String {
    let value = execute(
        &db.store(),
        &Op::DecisionsLog {
            project: "Mill".into(),
            include_superseded,
        },
    )
    .into_result()
    .expect("export");
    serde_json::from_value(value).expect("a string")
}

fn seed(db: &Database) {
    let store = db.store();
    let memories = Memories::new(&store);
    let mut old = decision("m-old", "Use SQLite", "2026-11-01T10:00:00Z");
    old.metadata = json!({"decision_ref": "D1", "rationale": "small | local"});
    old.superseded_by = Some("m-new".into());
    let mut new = decision(
        "m-new",
        "Use the engine store\nsecond line is not the title",
        "2026-11-03T10:00:00Z",
    );
    new.metadata = json!({"decision_ref": "D2", "rationale": "one writer"});
    new.capture_id = Some("cap-1".into());
    new.updated_at = "2026-11-05T09:00:00Z".into();
    // Captured later, but took effect first: event time orders the log.
    let mut early = decision("m-early", "Keep one binary", "2026-11-04T10:00:00Z");
    early.valid_from = Some("2026-10-30T00:00:00Z".into());
    early.outcome = Some("done".into());
    let mut secret = decision("m-secret", "secret", "2026-11-02T10:00:00Z");
    secret.sensitive = true;
    let other = {
        let mut row = decision("m-other", "elsewhere", "2026-11-02T10:00:00Z");
        row.project = Some("tick".into());
        row
    };
    let mut fact = NewMemory::new("m-fact", "a fact", "2026-11-02T10:00:00Z");
    fact.project = Some("mill".into());
    for row in [old, new, early, secret, other, fact] {
        memories.insert(&row).expect("insert");
    }
}

#[test]
fn live_log_lists_the_projects_decisions_by_event_date() {
    let db = Database::open_in_memory().expect("database");
    seed(&db);
    assert_eq!(
        export(&db, false),
        "# Decisions: Mill\n\n\
         Generated from remind-me on 2026-11-05; do not hand-edit.\n\n\
         | D | Date | Decision | Rationale | Status | Superseded by | Source |\n\
         |---|---|---|---|---|---|---|\n\
         | — | 2026-10-30 | Keep one binary | — | done | — | `m-early` |\n\
         | D2 | 2026-11-03 | Use the engine store | one writer | open | — | `m-new` · capture `cap-1` |\n"
    );
}

#[test]
fn superseded_decisions_are_kept_on_request_and_point_at_their_successor() {
    let db = Database::open_in_memory().expect("database");
    seed(&db);
    let log = export(&db, true);
    assert!(log.contains(
        "| D1 | 2026-11-01 | Use SQLite | small \\| local | superseded | D2 (`m-new`) | `m-old` |\n"
    ));
    assert_eq!(log, export(&db, true), "re-runs are byte-identical");
}

#[test]
fn a_project_without_decisions_renders_an_empty_log() {
    let db = Database::open_in_memory().expect("database");
    assert_eq!(
        export(&db, false),
        "# Decisions: Mill\n\nGenerated from remind-me; do not hand-edit.\n\n_No decisions recorded._\n"
    );
}

fn template_input(template: &str, project: Option<&str>) -> remind_me_core::models::ExportInput {
    remind_me_core::models::ExportInput {
        template: Some(template.into()),
        project: project.map(Into::into),
        ..Default::default()
    }
}

#[test]
fn export_memories_renders_the_decisions_template_inline() {
    let db = Database::open_in_memory().expect("database");
    seed(&db);
    let result = remind_me_core::export::export_memories(
        &db.store(),
        &template_input("decisions", Some("mill")),
    )
    .expect("export");
    assert_eq!(result.exported, 2);
    assert_eq!(result.template.as_deref(), Some("decisions"));
    assert!(result.entities.is_none());
    let content = result.content.expect("inline");
    assert!(content.starts_with("# Decisions: mill\n"));
    assert!(
        !content.contains("m-old"),
        "superseded only with include_deleted"
    );
}

#[test]
fn export_memories_refuses_an_unknown_template_or_a_missing_project() {
    let db = Database::open_in_memory().expect("database");
    let store = db.store();
    for input in [
        template_input("decision", Some("mill")),
        template_input("decisions", None),
        template_input("decisions", Some("  ")),
    ] {
        assert!(
            remind_me_core::export::export_memories(&store, &input).is_err(),
            "{input:?}"
        );
    }
}
