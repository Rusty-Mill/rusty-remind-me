//! `if_version` and `version` through `handle_request` (card #1487, pack
//! D120-D121): reads return the version, a stale one refuses the write with
//! the ADR-0028 `version_conflict` error and the current version.

use remind_me_mcp::McpServer;
use serde_json::{json, Value};

fn server(name: &str) -> McpServer {
    let dir = std::env::temp_dir().join(format!("rrm-version-test-{name}-{}", std::process::id()));
    remind_me_core::testing::use_full_tool_surface();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch wiki dir");
    McpServer::with_wiki(
        remind_me_core::Database::open_in_memory().unwrap(),
        remind_me_core::wiki_fs::Wiki::new(dir),
    )
}

/// `(is_error, text)` of one tool call.
fn call(s: &McpServer, tool: &str, args: Value) -> (bool, String) {
    let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                     "params":{"name":tool,"arguments":args}})
    .to_string();
    let r = s.handle_request(&req).expect("the server should answer");
    let text = r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    (r["result"]["isError"] == json!(true), text)
}

fn ok_json(s: &McpServer, tool: &str, args: Value) -> Value {
    let (failed, text) = call(s, tool, args);
    assert!(!failed, "{tool}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool}: {e}: {text}"))
}

fn conflict(s: &McpServer, tool: &str, args: Value) -> Value {
    let (failed, text) = call(s, tool, args);
    assert!(failed, "{tool} should refuse: {text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["code"], "version_conflict", "{body}");
    assert!(body["error"]["fix"].as_str().is_some());
    body
}

#[test]
fn update_refuses_a_stale_version_and_names_the_current_one() {
    let s = server("update");
    let added = ok_json(
        &s,
        "remind_me_add",
        json!({"content": "deploys on fridays", "response_format": "json"}),
    );
    let id = added["id"].as_str().unwrap();
    let read = added["version"].as_str().unwrap().to_string();
    let got = ok_json(&s, "remind_me_get", json!({"memory_id": id}));
    assert_eq!(got["version"], read.as_str(), "get and add agree");
    let batch = ok_json(&s, "remind_me_get", json!({"memory_ids": [id]}));
    assert_eq!(
        batch["memories"][0]["version"],
        read.as_str(),
        "batch get carries it too"
    );

    let updated = ok_json(
        &s,
        "remind_me_update",
        json!({"memory_id": id, "content": "deploys on thursdays", "if_version": read, "response_format": "json"}),
    );
    let now = updated["version"].as_str().unwrap();
    assert_ne!(now, read);

    let body = conflict(
        &s,
        "remind_me_update",
        json!({"memory_id": id, "content": "deploys on mondays", "if_version": read}),
    );
    assert_eq!(body["current_version"], now);
    assert!(body["updated_at"].as_str().is_some());
    let after = ok_json(&s, "remind_me_get", json!({"memory_id": id}));
    assert_eq!(after["content"], "deploys on thursdays");

    // Absent precondition: last write wins, as before.
    ok_json(
        &s,
        "remind_me_update",
        json!({"memory_id": id, "content": "deploys daily", "response_format": "json"}),
    );
}

#[test]
fn wiki_write_checks_and_wiki_read_returns_the_version() {
    let s = server("wiki");
    let written = ok_json(
        &s,
        "remind_me_wiki_write",
        json!({"title": "Deploys", "content": "Fridays.", "if_version": "absent"}),
    );
    let read = ok_json(&s, "remind_me_wiki_read", json!({"slug": "deploys"}));
    assert_eq!(read["version"], written["version"]);

    let body = conflict(
        &s,
        "remind_me_wiki_write",
        json!({"title": "Deploys", "content": "Mondays.", "if_version": "absent"}),
    );
    assert_eq!(body["current_version"], written["version"]);

    let (failed, text) = call(
        &s,
        "remind_me_wiki_write",
        json!({"title": "Deploys", "content": "Mondays.", "if_version": "yesterday"}),
    );
    assert!(failed && text.contains("invalid_input"), "{text}");
    let (failed, text) = call(
        &s,
        "remind_me_wiki_write",
        json!({"title": "Deploys", "content": "Mondays.", "if_version": 7}),
    );
    assert!(failed && text.contains("invalid_input"), "{text}");

    let (_, markdown) = call(
        &s,
        "remind_me_wiki_read",
        json!({"slug": "deploys", "response_format": "markdown"}),
    );
    let version = written["version"].as_str().unwrap();
    assert!(markdown.contains(version), "{markdown}");
}

#[test]
fn project_set_checks_the_version_get_returned() {
    let s = server("project");
    let unknown = ok_json(
        &s,
        "remind_me_project",
        json!({"action": "get", "project": "alpha"}),
    );
    let empty = unknown["version"].as_str().unwrap().to_string();
    ok_json(
        &s,
        "remind_me_project",
        json!({"action": "set", "project": "alpha", "stage": "idea", "status": "active",
               "because": "started", "if_version": empty}),
    );
    let body = conflict(
        &s,
        "remind_me_project",
        json!({"action": "set", "project": "alpha", "stage": "designed",
               "because": "drawn up", "if_version": empty}),
    );
    assert!(body.get("updated_at").is_none(), "{body}");
    assert!(body["current_version"].as_str().unwrap().starts_with("v1:"));
}
