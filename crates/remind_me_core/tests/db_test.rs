use remind_me_core::db::queries;
use remind_me_core::{Database, MemoryAddInput, MemorySearchInput};

#[test]
fn test_database_creation_and_add_memory() {
    let db = Database::open_in_memory().expect("Failed to open in-memory database");

    let add_input = MemoryAddInput {
        sensitive: false,
        content: "Rust implementation of remind_me with FTS5 search".to_string(),
        category: "project".to_string(),
        tags: vec!["rust".to_string(), "mcp".to_string()],
        source: "unit_test".to_string(),
        metadata: serde_json::json!({"test": true}),
        subject: Some("rusty_remind_me".to_string()),
        predicate: Some("uses".to_string()),
        object: Some("Rusty Mill".to_string()),
        entities: vec![],
    };

    let conn = db.conn();
    let mem = queries::add_memory(&conn, add_input).expect("Failed to add memory");
    assert!(!mem.id.is_empty());
    assert_eq!(mem.category, "project");

    let fetched = queries::get_memory_by_id(&conn, &mem.id)
        .expect("Failed to fetch memory")
        .expect("Memory not found");
    assert_eq!(fetched.content, mem.content);

    let search_input = MemorySearchInput {
        strategy: Default::default(),
        include_sensitive: false,
        query: "FTS5".to_string(),
        category: None,
        tags: None,
        limit: 10,
        token_budget: 800,
        response_format: Default::default(),
        include_dormant: false,
        min_vitality: 0.0,
        verbose: false,
        expand_entities: false,
        include_neighbors: false,
        expand_co_retrieval: false,
        bootstrap: false,
    };

    let search_results = queries::search_memories(&conn, &search_input).expect("Search failed");
    assert!(!search_results.is_empty());
    assert_eq!(search_results[0].memory.id, mem.id);
}
