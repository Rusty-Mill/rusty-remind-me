//! Complete impact inventories come from the existing engine-backed exporter.
use remind_me_core::daemon::ops::{execute, Op};
use remind_me_core::db::memories::{Memories, MemoryEdit, NewMemory};
use remind_me_core::Database;
use serde_json::{json, Value};

#[test]
fn inventory_retains_archived_superseded_and_deleted_requirements_without_rank_limits() {
    let db = Database::open_in_memory().expect("database");
    let store = db.store();
    let memories = Memories::new(&store);
    for index in 0..125 {
        let mut row = NewMemory::new(
            format!("requirement-{index:03}"),
            "durable contract",
            "revision-1",
        );
        row.metadata = json!({"factory_requirement": {"binding": true, "repository": "mill",
            "projects": ["inactive"], "packages": ["rusty_atomic_file"]}});
        if index == 0 {
            row.status = "archived".into();
        }
        if index == 1 {
            row.superseded_by = Some("requirement-002".into());
        }
        if index == 2 {
            row.deleted_at = Some("deleted".into());
        }
        memories.insert(&row).expect("insert");
    }
    let snapshot: Value = execute(&store, &Op::ImpactExport { query: None })
        .into_result()
        .expect("export");
    assert_eq!(snapshot["complete"], true);
    assert_eq!(snapshot["records"].as_array().expect("records").len(), 125);
    assert_eq!(snapshot["records"][0]["status"], "archived");
    assert_eq!(snapshot["records"][1]["superseded_by"], "requirement-002");
    assert_eq!(snapshot["records"][2]["deleted_at"], "deleted");
    assert_eq!(snapshot["retrieved_context"], json!([]));
    let mut edit = MemoryEdit::at("revision-2");
    edit.content = Some("changed durable contract".into());
    memories.apply_edit("requirement-000", &edit).expect("edit");
    let after = remind_me_core::export::impact_snapshot(&store, None).expect("snapshot");
    assert_eq!(after["records"][0]["updated_at"], "revision-2");
    assert_eq!(after["records"][0]["content"], "changed durable contract");
}

#[test]
fn supplemental_retrieval_cannot_truncate_inventory() {
    let db = Database::open_in_memory().expect("database");
    let store = db.store();
    Memories::new(&store)
        .insert(&NewMemory::new(
            "binding",
            "preserve atomicity",
            "revision-1",
        ))
        .expect("insert");
    let snapshot = remind_me_core::export::impact_snapshot(&store, Some("unmatched words xyzzy"))
        .expect("snapshot");
    assert_eq!(snapshot["records"].as_array().expect("records").len(), 1);
    assert_eq!(snapshot["records"][0]["id"], "binding");
}
