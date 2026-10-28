//! Schema adaptation of historical import oracles, without a database server.
#[allow(dead_code)]
#[path = "suite/recorded.rs"]
mod recorded;

use serde_json::{json, Value};

#[test]
fn historical_memories_gain_only_explicit_v32_defaults() {
    let historical: Value =
        serde_json::from_str(include_str!("fixtures/legacy_postgres_migrated.json")).unwrap();
    let adapted = recorded::with_v32_memory_defaults(historical.clone());
    for (old, new) in historical
        .as_array()
        .unwrap()
        .iter()
        .zip(adapted.as_array().unwrap())
    {
        let old = old.as_object().unwrap();
        let new = new.as_object().unwrap();
        assert_eq!(new.len(), old.len() + 13);
        for (key, value) in old {
            assert_eq!(&new[key], value, "historical field {key}");
        }
        assert_eq!(new["confidence"], json!(1.0));
        assert_eq!(new["written_by"], "unknown");
        assert_eq!(new["capture_method"], "manual");
        for key in [
            "project",
            "session_id",
            "git_remote",
            "git_branch",
            "git_sha",
            "cwd",
            "valid_from",
            "valid_until",
            "verified_at",
            "outcome",
        ] {
            assert_eq!(new[key], Value::Null, "default for {key}");
        }
    }
}

#[test]
fn adaptation_preserves_existing_values_tombstones_metadata_and_graph() {
    let memory = json!({
        "id": "gone", "content": "historical tombstone", "hub_seq": 42,
        "deleted_at": "2026-08-02T00:00:00Z", "tags": ["kept"],
        "subject": "s", "predicate": "p", "object": "o", "origin_node": "node-a",
        "confidence": 0.25, "written_by": "author", "capture_method": "import",
        "project": "project", "valid_until": null,
        "metadata": {"id": "nested", "content": "opaque", "hub_seq": 99}
    });
    let graph = json!({"entities": [{"id": "e", "name": "entity"}],
        "links": [{"id": "gone|e", "created_at": "then"}],
        "relations": [{"id": "r", "metadata": {"key": "value"}}]});
    let original = json!({"memories": [memory.clone()], "graph": graph.clone()});
    let adapted = recorded::with_v32_memory_defaults(original);
    for (key, value) in memory.as_object().unwrap() {
        assert_eq!(&adapted["memories"][0][key], value, "preserve {key}");
    }
    assert_eq!(adapted["graph"], graph);
    assert_eq!(recorded::with_v32_memory_defaults(adapted.clone()), adapted);
}
