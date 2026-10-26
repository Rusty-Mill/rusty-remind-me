//! Profile enforcement at the MCP protocol surface (gap E3, issue #122;
//! ADR-0027).
//!
//! Its own binary: the profile is a process-wide env var, and the inline unit
//! tests in `lib.rs` assume the default `full` surface. Setting it there would
//! quietly shrink what those tests see.

#[path = "../../remind_me_core/src/test_env.rs"]
mod test_env;

use remind_me_core::tool_profiles::{ALIASES, CORE, GROUPS, TOOL_PROFILE_ENV, WIKI};
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

/// Run `body` under `profile` (`None` = unset), restoring the env after.
fn with_profile<T>(profile: Option<&str>, body: impl FnOnce() -> T) -> T {
    let _guard = env_lock();
    match profile {
        Some(p) => crate::test_env::set_var(TOOL_PROFILE_ENV, p),
        None => crate::test_env::remove_var(TOOL_PROFILE_ENV),
    }
    let out = body();
    crate::test_env::remove_var(TOOL_PROFILE_ENV);
    out
}

fn server() -> McpServer {
    McpServer::new(Database::open_in_memory().unwrap())
}

fn listed(server: &McpServer) -> Vec<Value> {
    let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let resp = server.handle_request(&req.to_string()).unwrap();
    resp["result"]["tools"]
        .as_array()
        .expect("a tools array")
        .clone()
}

fn names(server: &McpServer) -> Vec<String> {
    listed(server)
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

fn call(server: &McpServer, name: &str, arguments: Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    server.handle_request(&req.to_string()).unwrap()["result"].clone()
}

fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn full_advertises_every_grouped_tool_in_group_order_and_no_alias() {
    let tools = with_profile(None, || names(&server()));

    let expected: Vec<&str> = GROUPS.iter().flat_map(|(_, t)| t.iter().copied()).collect();
    assert_eq!(
        tools, expected,
        "every grouped tool has a definition, in order"
    );
    for (old, _) in ALIASES {
        assert!(
            !tools.iter().any(|t| t == old),
            "{old} is an alias but listed"
        );
    }
}

#[test]
fn core_lists_exactly_the_ten_core_tools() {
    let tools = with_profile(Some("core"), || names(&server()));
    assert_eq!(tools, CORE);
}

#[test]
fn a_comma_list_adds_groups_to_core() {
    let tools = with_profile(Some("core,wiki"), || names(&server()));
    let expected: Vec<&str> = CORE.iter().chain(WIKI.iter()).copied().collect();
    assert_eq!(tools, expected);
}

#[test]
fn standard_keeps_pipeline_and_wiki_but_drops_ops() {
    let tools = with_profile(Some("standard"), || names(&server()));
    assert!(tools.iter().any(|t| t == "remind_me_queue"));
    assert!(tools.iter().any(|t| t == "remind_me_wiki_read"));
    assert!(!tools.iter().any(|t| t == "remind_me_import"));
    assert!(!tools.iter().any(|t| t == "remind_me_sync_status"));
}

#[test]
fn a_hidden_tool_is_refused_on_call_and_the_refusal_names_its_group() {
    let result = with_profile(Some("core"), || {
        call(&server(), "remind_me_import", json!({}))
    });

    // This is the criterion that separates a profile from a documentation
    // change: a model that guessed the name must not reach it.
    assert_eq!(result["isError"], true);
    let text = text(&result);
    assert!(text.contains("not available"), "unhelpful refusal: {text}");
    assert!(text.contains("'import' group"), "no group named: {text}");
    assert!(
        text.contains(&format!("{TOOL_PROFILE_ENV}=core,import")),
        "no remedy: {text}"
    );
}

#[test]
fn an_alias_follows_its_replacements_group() {
    let (hidden, visible) = with_profile(Some("core"), || {
        let s = server();
        (
            call(&s, "remind_me_import_chat", json!({})),
            call(&s, "remind_me_stats", json!({})),
        )
    });
    // import_chat merged into import, which core hides: the old name must not
    // be a back door. stats merged into digest, which core keeps.
    assert_eq!(hidden["isError"], true);
    assert!(text(&hidden).contains("'import' group"));
    assert_ne!(
        visible["isError"], true,
        "core broke an alias of a core tool: {visible}"
    );
}

#[test]
fn every_alias_still_reaches_a_handler() {
    let unknown: Vec<&str> = with_profile(None, || {
        let s = server();
        ALIASES
            .iter()
            .map(|(old, _)| *old)
            .filter(|old| text(&call(&s, old, json!({}))).starts_with("Unknown tool"))
            .collect()
    });
    assert!(unknown.is_empty(), "aliases with no handler: {unknown:?}");
}

#[test]
fn a_merged_tool_refuses_a_bad_discriminator_as_invalid_input() {
    let result = with_profile(None, || {
        call(&server(), "remind_me_queue", json!({"kind": "nope"}))
    });
    assert_eq!(result["isError"], true);
    let text = text(&result);
    assert!(text.starts_with("Invalid input"), "{text}");
    assert!(text.contains("decompose"), "lists the kinds: {text}");
}

#[test]
fn every_listed_tool_is_callable_under_the_same_profile() {
    with_profile(Some("core"), || {
        let s = server();
        // List and call must agree. A tool advertised but refused is the same
        // class of bug as one hidden but reachable, just in the other direction.
        for name in names(&s) {
            let text = text(&call(&s, &name, json!({})));
            assert!(
                !text.contains("not available under the"),
                "{name} was listed but refused by the profile"
            );
        }
    });
}

#[test]
fn server_status_reports_the_active_profile() {
    let report = with_profile(Some("core,wiki"), || {
        call(
            &server(),
            "remind_me_server_status",
            json!({"response_format": "json"}),
        )
    });
    let report: Value = serde_json::from_str(&text(&report)).expect("json status");
    assert_eq!(report["tool_profile"]["profile"], "core,wiki");
    assert_eq!(report["tool_profile"]["groups"], json!(["core", "wiki"]));
}

#[test]
fn every_description_leads_with_remind_me_and_non_core_names_its_group() {
    // Clients that defer tools find them by keyword search over name and
    // description, and a search for "remind me" must land on these (#52).
    let tools = with_profile(None, || listed(&server()));
    for tool in &tools {
        let name = tool["name"].as_str().unwrap();
        let description = tool["description"].as_str().unwrap_or_default();
        let group = GROUPS
            .iter()
            .find(|(_, t)| t.contains(&name))
            .map(|(g, _)| *g)
            .unwrap();
        if group == "core" {
            let lead = description.split(". ").next().unwrap_or_default();
            assert!(
                lead.contains("remind-me"),
                "{name}: core lead lacks remind-me: {description}"
            );
        } else {
            let label = format!("remind-me {group}: ");
            assert!(
                description.starts_with(&label),
                "{name} should start {label:?}: {description}"
            );
        }
    }
}
