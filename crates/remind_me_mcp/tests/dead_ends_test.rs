//! Dead ends through `handle_request` (card #1484): search renders the
//! warning above the hits, and resolving as abandoned suggests one (D123).

use remind_me_mcp::McpServer;
use serde_json::{json, Value};

fn server() -> McpServer {
    let dir = std::env::temp_dir().join(format!("rrm-dead-ends-test-{}", std::process::id()));
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

#[test]
fn a_dead_end_warns_in_search_and_resolve_suggests_recording_one() {
    let s = server();
    let (failed, added) = call(
        &s,
        "remind_me_add",
        json!({"content": "use jemalloc for the musl build", "memory_type": "decision",
               "metadata": {"rationale": "faster"}, "response_format": "json"}),
    );
    assert!(!failed, "{added}");
    let decision: Value = serde_json::from_str(&added).unwrap();
    let id = decision["id"].as_str().unwrap();

    let (failed, resolved) = call(
        &s,
        "remind_me_resolve",
        json!({"memory_id": id, "outcome": "abandoned", "response_format": "json"}),
    );
    assert!(!failed, "{resolved}");
    let resolved: Value = serde_json::from_str(&resolved).unwrap();
    assert!(resolved["dead_end_hint"].as_str().unwrap().contains(id));

    let (failed, text) = call(
        &s,
        "remind_me_add",
        json!({"content": "rebuild musl with jemalloc", "memory_type": "dead_end",
               "metadata": {"tried": "rebuild musl with jemalloc", "why_failed": "segfaults", "from": id}}),
    );
    assert!(!failed, "{text}");

    let (failed, md) = call(
        &s,
        "remind_me_search",
        json!({"query": "rebuild musl jemalloc"}),
    );
    assert!(!failed, "{md}");
    assert!(md.contains("Dead ends") && md.contains("segfaults"), "{md}");

    let (_, compact) = call(
        &s,
        "remind_me_search",
        json!({"query": "rebuild musl jemalloc", "response_format": "json"}),
    );
    let compact: Value = serde_json::from_str(&compact).unwrap();
    assert_eq!(compact["warnings"][0]["why_failed"], "segfaults");
}

#[test]
fn a_dead_end_without_why_failed_is_refused() {
    let s = server();
    let (failed, text) = call(
        &s,
        "remind_me_add",
        json!({"content": "x", "memory_type": "dead_end", "metadata": {"tried": "x"}}),
    );
    assert!(failed && text.contains("why_failed"), "{text}");
}
