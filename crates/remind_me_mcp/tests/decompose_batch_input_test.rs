//! `remind_me_decompose_batch` keeps serde defaults for omitted fields but
//! rejects invalid supplied ones, through the alias and through
//! `remind_me_queue(kind=decompose)` alike (ADR-0029).

use remind_me_mcp::McpServer;
use serde_json::{json, Value};

fn server() -> McpServer {
    let dir = std::env::temp_dir().join(format!(
        "rrm-decompose-batch-test-{}-{:?}",
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

fn call(s: &McpServer, tool: &str, args: Value) -> Value {
    let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                     "params":{"name":tool,"arguments":args}})
    .to_string();
    s.handle_request(&req).expect("the server should answer")["result"].clone()
}

fn rejected(result: &Value) -> bool {
    result["isError"] == json!(true)
        && result["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("Invalid input"))
}

/// Each invalid shape, as the alias and as the consolidated queue spell it.
fn invalid_calls() -> Vec<(&'static str, Value)> {
    let bad = [
        json!({"idle_minutes": -1}),
        json!({"idle_minutes": "120"}),
        json!({"idle_minutes": 1u64 << 33}),
    ];
    let mut calls = Vec::new();
    for args in bad {
        let mut queued = args.clone();
        queued["kind"] = json!("decompose");
        calls.push(("remind_me_decompose_batch", args));
        calls.push(("remind_me_queue", queued));
    }
    calls.push((
        "remind_me_decompose_batch",
        json!({"batch_size": "twenty", "idle_minutes": 120}),
    ));
    calls.push((
        "remind_me_queue",
        json!({"kind": "decompose", "limit": "twenty", "idle_minutes": 120}),
    ));
    calls
}

#[test]
fn invalid_supplied_fields_are_rejected_through_alias_and_queue() {
    let s = server();
    for (tool, args) in invalid_calls() {
        let result = call(&s, tool, args.clone());
        assert!(rejected(&result), "{tool} {args} -> {result}");
    }
}

#[test]
fn valid_and_omitted_fields_are_accepted_through_alias_and_queue() {
    let s = server();
    for (tool, args) in [
        ("remind_me_decompose_batch", json!({})),
        (
            "remind_me_decompose_batch",
            json!({"batch_size": 5, "idle_minutes": 120}),
        ),
        ("remind_me_queue", json!({"kind": "decompose"})),
        (
            "remind_me_queue",
            json!({"kind": "decompose", "limit": 20, "idle_minutes": 120}),
        ),
    ] {
        let result = call(&s, tool, args.clone());
        assert_ne!(result["isError"], json!(true), "{tool} {args} -> {result}");
    }
}

#[test]
fn the_listed_queue_schema_advertises_the_u32_maximum() {
    let s = server();
    let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string();
    let listed = s.handle_request(&req).expect("the server should answer");
    let queue = listed["result"]["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|t| t["name"] == "remind_me_queue"))
        .expect("remind_me_queue is listed");
    let idle = &queue["inputSchema"]["properties"]["idle_minutes"];
    assert_eq!(idle["maximum"], json!(u32::MAX), "{idle}");
}
