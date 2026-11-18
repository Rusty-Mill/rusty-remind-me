//! `remind_me_export_memories` with `template` (card #1565, pack D22): the
//! decisions log over MCP, failing closed on malformed arguments.

use remind_me_mcp::McpServer;
use serde_json::{json, Value};

fn server(tag: &str) -> McpServer {
    remind_me_core::testing::use_full_tool_surface();
    let dir = std::env::temp_dir().join(format!("rrm-export-tpl-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch wiki dir");
    McpServer::with_wiki(
        remind_me_core::Database::open_in_memory().unwrap(),
        remind_me_core::wiki_fs::Wiki::new(dir),
    )
}

fn call(s: &McpServer, tool: &str, args: Value) -> Value {
    let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                     "params":{"name":tool,"arguments":args}})
    .to_string();
    s.handle_request(&req).expect("the server should answer")["result"].clone()
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or_default()
}

fn add_decision(s: &McpServer, content: &str, project: &str, sensitive: bool) {
    let added = call(
        s,
        "remind_me_add",
        json!({"content": content, "memory_type": "decision", "project": project,
               "sensitive": sensitive, "metadata": {"rationale": "because"}}),
    );
    assert_ne!(added["isError"], true, "{}", text(&added));
}

#[test]
fn renders_only_the_projects_non_sensitive_decisions() {
    let s = server("render");
    add_decision(&s, "Ship the export", "mill", false);
    add_decision(&s, "Secret choice", "mill", true);
    add_decision(&s, "Other project choice", "tick", false);

    let result = call(
        &s,
        "remind_me_export_memories",
        json!({"template": "decisions", "project": "mill", "category": "ignored"}),
    );

    assert_ne!(result["isError"], true, "{}", text(&result));
    let body: Value = serde_json::from_str(text(&result)).unwrap();
    assert_eq!(body["template"], "decisions");
    assert_eq!(body["exported"], 1);
    let log = body["content"].as_str().unwrap();
    assert!(log.contains("Ship the export"), "{log}");
    assert!(
        !log.contains("Secret choice") && !log.contains("Other project"),
        "{log}"
    );
}

#[test]
fn malformed_arguments_are_refused_not_exported_in_full() {
    let s = server("malformed");
    add_decision(&s, "Ship the export", "mill", false);
    for args in [
        json!({"template": 5, "project": "mill"}),
        json!({"template": "decisions", "project": ["mill"]}),
        json!({"template": "decisions", "project": "mill", "include_deleted": "yes"}),
        json!({"template": "decision", "project": "mill"}),
        json!({"template": "decisions"}),
    ] {
        let result = call(&s, "remind_me_export_memories", args.clone());
        assert_eq!(result["isError"], true, "{args}: {}", text(&result));
        let why = text(&result);
        assert!(
            why.starts_with("Invalid arguments") || why.starts_with("Export error"),
            "{args}: {why}"
        );
        assert!(!why.contains("Ship the export"), "{args}");
    }
}

#[test]
fn a_file_target_outside_the_export_roots_is_refused() {
    let s = server("roots");
    let result = call(
        &s,
        "remind_me_export_memories",
        json!({"template": "decisions", "project": "mill", "file_path": "/etc/remind-me-decisions.md"}),
    );
    assert_eq!(result["isError"], true, "{}", text(&result));
    assert!(text(&result).contains("export roots"), "{}", text(&result));
}
