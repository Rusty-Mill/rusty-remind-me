//! `remind_me_brief` through `handle_request`: the text a caller receives,
//! its error shape (ADR-0028 §3), and its strict input.

use remind_me_mcp::McpServer;
use serde_json::{json, Value};

fn server() -> McpServer {
    let dir = std::env::temp_dir().join(format!(
        "rrm-brief-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
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

fn seeded() -> (McpServer, String) {
    let s = server();
    let (failed, _) = call(&s, "remind_me_entity_upsert", json!({"name": "quokka"}));
    assert!(!failed);
    let (failed, added) = call(
        &s,
        "remind_me_add",
        json!({"content": "shipped the quokka parser", "category": "general", "tags": ["quokka"]}),
    );
    assert!(!failed, "{added}");
    let id = added
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .find(|w| w.starts_with("mem_"))
        .unwrap_or_else(|| panic!("no id in {added}"))
        .to_string();
    (s, id)
}

#[test]
fn a_known_project_returns_json_sections_with_ids() {
    let (s, id) = seeded();
    let (failed, text) = call(
        &s,
        "remind_me_brief",
        json!({"project": "Quokka", "response_format": "json"}),
    );
    assert!(!failed, "{text}");
    let brief: Value = serde_json::from_str(&text).unwrap();
    let names: Vec<&str> = brief["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["state", "open", "activity"]);
    assert_eq!(brief["sections"][2]["items"][0]["id"], json!(id));
    assert_eq!(brief["project"]["name"], json!("quokka"));
    assert_eq!(brief["budget"]["limit"], json!(1500));
}

#[test]
fn markdown_is_a_view_of_the_same_brief() {
    let (s, id) = seeded();
    let (failed, text) = call(
        &s,
        "remind_me_brief",
        json!({"project": "quokka", "response_format": "markdown"}),
    );
    assert!(!failed, "{text}");
    assert!(text.starts_with("# Brief: quokka"), "{text}");
    assert!(text.contains(&id));
    assert!(text.contains("## Current state\n_None._"), "{text}");
}

#[test]
fn an_unknown_project_is_an_error_with_suggestions() {
    let (s, _) = seeded();
    let (failed, text) = call(&s, "remind_me_brief", json!({"project": "quoka"}));
    assert!(failed);
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["code"], json!("unknown_project"));
    assert!(body["error"]["fix"]
        .as_str()
        .unwrap()
        .contains("suggestions"));
    assert_eq!(body["suggestions"], json!(["quokka"]));
}

#[test]
fn input_is_validated_at_the_boundary() {
    let (s, _) = seeded();
    for args in [
        json!({}),
        json!({"project": "   "}),
        json!({"project": "quokka", "sinse": "2d"}),
        json!({"project": "quokka", "since": "soon"}),
        json!({"project": "quokka", "token_budget": 10}),
    ] {
        let (failed, text) = call(&s, "remind_me_brief", args.clone());
        assert!(failed, "{args} accepted");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["error"]["code"], json!("invalid_input"), "{args}");
    }
}

#[test]
fn the_tool_is_listed_with_a_strict_schema() {
    let s = server();
    let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string();
    let r = s.handle_request(&req).unwrap();
    let tool = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("remind_me_brief"))
        .expect("remind_me_brief listed")
        .clone();
    assert_eq!(tool["inputSchema"]["additionalProperties"], json!(false));
    assert_eq!(tool["inputSchema"]["required"], json!(["project"]));
    assert!(tool["description"].as_str().unwrap().len() <= 160);
}
