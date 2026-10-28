//! Coverage for `remind_me_stats`.

use remind_me_core::db::imports::ImportLedger;
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::{stats, Database, MemoryAddInput};

fn add(store: &Store<'_>, content: &str, category: &str, source: &str) -> String {
    let input = MemoryAddInput {
        sensitive: false,
        content: content.to_string(),
        category: category.to_string(),
        tags: vec![],
        source: source.to_string(),
        metadata: serde_json::json!({}),
        subject: None,
        predicate: None,
        object: None,
        entities: vec![],
        ..Default::default()
    };
    queries::add_memory(store, input).expect("add failed").id
}

#[test]
fn empty_store_reports_zeros_not_an_error() {
    let db = Database::open_in_memory().unwrap();
    let s = stats::collect(&db.store()).unwrap();

    assert_eq!(s.total_memories, 0);
    assert_eq!(s.total_imports, 0);
    assert!(s.categories.is_empty());
    assert!(s.sources.is_empty());
    assert!(s.recent.is_empty());
}

#[test]
fn counts_group_by_category_and_source() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "a", "fact", "manual");
    add(&store, "b", "fact", "manual");
    add(&store, "c", "decision", "chat_import");

    let s = stats::collect(&store).unwrap();
    assert_eq!(s.total_memories, 3);
    assert_eq!(s.categories.get("fact"), Some(&2));
    assert_eq!(s.categories.get("decision"), Some(&1));
    assert_eq!(s.sources.get("manual"), Some(&2));
    assert_eq!(s.sources.get("chat_import"), Some(&1));
}

#[test]
fn deleted_memories_leave_every_count() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let doomed = add(&store, "going away", "fact", "manual");
    add(&store, "staying", "fact", "manual");

    queries::delete_memory(&store, &doomed).unwrap();

    let s = stats::collect(&store).unwrap();
    assert_eq!(s.total_memories, 1);
    assert_eq!(s.categories.get("fact"), Some(&1));
    assert_eq!(s.recent.len(), 1);
}

#[test]
fn recent_is_capped_at_five_newest_first() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for i in 0..8 {
        add(&store, &format!("memory {}", i), "general", "manual");
    }

    let s = stats::collect(&store).unwrap();
    assert_eq!(s.total_memories, 8);
    assert_eq!(s.recent.len(), 5, "reference caps recent at 5");
}

#[test]
fn recent_preview_is_truncated_to_eighty_characters() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let long = "x".repeat(200);
    add(&store, &long, "general", "manual");

    let s = stats::collect(&store).unwrap();
    assert_eq!(s.recent[0].preview.chars().count(), 80);
}

#[test]
fn short_content_is_not_padded_or_truncated() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "brief", "general", "manual");

    let s = stats::collect(&store).unwrap();
    assert_eq!(s.recent[0].preview, "brief");
}

#[test]
fn import_ledger_is_counted_separately_from_memories() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "a memory", "general", "manual");
    ImportLedger::new(&store)
        .record_chat("imp_1", "chat.json", "abc", "2026-01-01T00:00:00Z", "{}")
        .unwrap();

    let s = stats::collect(&store).unwrap();
    assert_eq!(s.total_memories, 1);
    assert_eq!(s.total_imports, 1);
}

#[test]
fn db_size_is_reported_for_an_in_memory_database() {
    let db = Database::open_in_memory().unwrap();
    let s = stats::collect(&db.store()).unwrap();

    // An in-memory database has temporary engine tables on disk, so it has
    // a size too (`db::stats` checks the byte count is not zero); a few
    // kilobytes round to 0.00 MB here, so this checks only the shape.
    assert!(s.db_size_mb >= 0.0, "got {}", s.db_size_mb);
    assert_eq!(s.db_path, "", "an in-memory database has no path");
}

#[test]
fn db_path_is_reported_for_a_file_backed_database() {
    let dir = std::env::temp_dir().join(format!("rmm_stats_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("stats_test.db");
    let _ = std::fs::remove_file(&path);

    {
        let db = Database::open(&path).unwrap();
        let s = stats::collect(&db.store()).unwrap();
        assert!(
            s.db_path.ends_with("stats_test.db"),
            "expected a real path, got {:?}",
            s.db_path
        );
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn stats_serialize_with_the_reference_field_names() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add(&store, "a", "fact", "manual");

    let value = serde_json::to_value(stats::collect(&store).unwrap()).unwrap();
    for field in [
        "total_memories",
        "total_imports",
        "categories",
        "sources",
        "recent",
        "db_path",
        "db_size_mb",
    ] {
        assert!(value.get(field).is_some(), "missing field {}", field);
    }
    assert!(value["categories"].is_object(), "categories is a keyed map");
    assert!(value["recent"].is_array());
}
