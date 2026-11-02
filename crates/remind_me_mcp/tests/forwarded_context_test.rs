//! #592: a call forwarded by the remote connector (it names its caller under
//! `CALLER_META_KEY`) is stamped with no directory, git or session context:
//! the server's own would be someone else's. An explicit `project` still
//! lands.

use remind_me_mcp::{McpServer, CALLER_META_KEY};
use serde_json::{json, Value};

fn server() -> McpServer {
    let dir = std::env::temp_dir().join(format!("rrm-fwd-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch wiki dir");
    McpServer::with_wiki(
        remind_me_core::Database::open_in_memory().unwrap(),
        remind_me_core::wiki_fs::Wiki::new(dir),
    )
}

fn add(s: &McpServer, args: Value, forwarded: bool) -> Value {
    let mut params = json!({ "name": "remind_me_add", "arguments": args });
    if forwarded {
        params["_meta"] = json!({ CALLER_META_KEY: "claude-ai/1.0" });
    }
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params});
    let r = s.handle_request(&req.to_string()).expect("an answer");
    let text = r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    serde_json::from_str(text).unwrap_or_else(|_| panic!("json record: {text}"))
}

#[test]
fn a_forwarded_add_has_no_server_context_but_keeps_an_explicit_project() {
    let s = server();
    let plain = add(&s, json!({ "content": "from the connector" }), true);
    for column in [
        "project",
        "cwd",
        "git_remote",
        "git_branch",
        "git_sha",
        "session_id",
    ] {
        assert_eq!(plain[column], Value::Null, "{column}: {plain}");
    }
    let named = add(
        &s,
        json!({ "content": "named", "project": "let-em-loose" }),
        true,
    );
    assert_eq!(named["project"], "let-em-loose");
    assert_eq!(named["cwd"], Value::Null);
}

#[test]
fn a_local_add_still_detects_its_directory() {
    let s = server();
    let local = add(&s, json!({ "content": "local" }), false);
    assert_ne!(local["cwd"], Value::Null, "{local}");
}
