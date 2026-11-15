//! #223: `remind_me_list` with `limit: 0` is a validation error, not one row.

use remind_me_mcp::McpServer;
use serde_json::json;

fn server() -> McpServer {
    let dir = std::env::temp_dir().join(format!("rrm-list-limit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch wiki dir");
    McpServer::with_wiki(
        remind_me_core::Database::open_in_memory().unwrap(),
        remind_me_core::wiki_fs::Wiki::new(dir),
    )
}

fn call(s: &McpServer, tool: &str, args: serde_json::Value) -> serde_json::Value {
    let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                     "params":{"name":tool,"arguments":args}})
    .to_string();
    s.handle_request(&req).expect("the server should answer")["result"].clone()
}

#[test]
fn a_zero_limit_is_an_error_naming_the_bound() {
    let s = server();
    call(&s, "remind_me_add", json!({"content": "one"}));
    call(&s, "remind_me_add", json!({"content": "two"}));

    let result = call(&s, "remind_me_list", json!({"limit": 0}));

    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("limit must be between 1 and 100"), "{text}");
}

#[test]
fn an_omitted_limit_lists_every_row() {
    let s = server();
    call(&s, "remind_me_add", json!({"content": "one"}));
    call(&s, "remind_me_add", json!({"content": "two"}));

    let result = call(&s, "remind_me_list", json!({"response_format": "json"}));

    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    let page: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(page["count"], 2);
}
