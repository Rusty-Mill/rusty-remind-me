//! Profile enforcement at the MCP protocol surface (gap E3, issue #122).
//!
//! Its own binary: the profile is a process-wide env var, and the inline unit
//! tests in `lib.rs` assume the default `full` surface. Setting it there would
//! quietly shrink what those tests see.

#[path = "../../remind_me_core/src/test_env.rs"]
mod test_env;

use remind_me_core::tool_profiles::TOOL_PROFILE_ENV;
use remind_me_core::Database;
use remind_me_mcp::McpServer;
use serde_json::{json, Value};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn server() -> McpServer {
    McpServer::new(Database::open_in_memory().unwrap())
}

fn listed_tools(server: &McpServer) -> Vec<String> {
    let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let resp = server.handle_request(&req.to_string()).unwrap();
    resp["result"]["tools"]
        .as_array()
        .expect("a tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

fn call(server: &McpServer, name: &str) -> Value {
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": name, "arguments": {} }
    });
    server.handle_request(&req.to_string()).unwrap()["result"].clone()
}

#[test]
fn full_advertises_everything() {
    let _guard = env_lock();
    crate::test_env::remove_var(TOOL_PROFILE_ENV);

    let tools = listed_tools(&server());

    // The default must not narrow anything, or upgrading silently removes
    // tools from an existing install.
    assert!(tools.len() > 50, "full listed only {} tools", tools.len());
    assert!(tools.iter().any(|t| t == "remind_me_import_chat"));
    assert!(tools.iter().any(|t| t == "remind_me_search"));
}

#[test]
fn core_lists_only_the_conversational_surface() {
    let _guard = env_lock();
    crate::test_env::set_var(TOOL_PROFILE_ENV, "core");

    let tools = listed_tools(&server());

    assert!(tools.iter().any(|t| t == "remind_me_search"));
    assert!(tools.iter().any(|t| t == "remind_me_server_status"));
    assert!(!tools.iter().any(|t| t == "remind_me_import_chat"));
    assert!(!tools.iter().any(|t| t == "remind_me_consolidate"));
    assert!(
        tools.len() < 20,
        "core listed {} tools — the whole point is a small surface",
        tools.len()
    );
    crate::test_env::remove_var(TOOL_PROFILE_ENV);
}

#[test]
fn standard_keeps_maintenance_but_drops_ops() {
    let _guard = env_lock();
    crate::test_env::set_var(TOOL_PROFILE_ENV, "standard");

    let tools = listed_tools(&server());

    assert!(tools.iter().any(|t| t == "remind_me_consolidate"));
    assert!(tools.iter().any(|t| t == "remind_me_search"));
    assert!(!tools.iter().any(|t| t == "remind_me_import_chat"));
    assert!(!tools.iter().any(|t| t == "remind_me_sync_status"));
    crate::test_env::remove_var(TOOL_PROFILE_ENV);
}

#[test]
fn a_hidden_tool_is_refused_on_call_not_merely_undocumented() {
    let _guard = env_lock();
    crate::test_env::set_var(TOOL_PROFILE_ENV, "core");

    let result = call(&server(), "remind_me_import_chat");

    // This is the criterion that separates a profile from a documentation
    // change. Undocumented only, a model that guessed the name would still
    // reach it and the caller would never know their surface was porous.
    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("not available"), "unhelpful refusal: {text}");
    // The refusal names the way out, so a caller who trimmed too far is not
    // left guessing why a documented tool vanished.
    assert!(text.contains(TOOL_PROFILE_ENV), "no remedy offered: {text}");
    crate::test_env::remove_var(TOOL_PROFILE_ENV);
}

#[test]
fn a_visible_tool_still_works_under_a_narrowed_profile() {
    let _guard = env_lock();
    crate::test_env::set_var(TOOL_PROFILE_ENV, "core");

    // Narrowing must not break what it keeps.
    let result = call(&server(), "remind_me_stats");

    assert_ne!(result["isError"], true, "core broke a core tool: {result}");
    crate::test_env::remove_var(TOOL_PROFILE_ENV);
}

#[test]
fn every_listed_tool_is_callable_under_the_same_profile() {
    let _guard = env_lock();
    crate::test_env::set_var(TOOL_PROFILE_ENV, "core");
    let server = server();

    // List and call must agree. A tool advertised but refused is the same
    // class of bug as one hidden but reachable, just in the other direction.
    for name in listed_tools(&server) {
        let result = call(&server, &name);
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        assert!(
            !text.contains("not available under the"),
            "{name} was listed but refused by the profile"
        );
    }
    crate::test_env::remove_var(TOOL_PROFILE_ENV);
}
