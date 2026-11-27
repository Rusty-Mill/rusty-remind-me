//! Profile enforcement at the MCP protocol surface (gap E3, issue #122;
//! ADR-0030).
//!
//! Its own binary: the profile is a process-wide env var, and the inline unit
//! tests in `lib.rs` assume the whole surface when it is unset. Setting it there would
//! quietly shrink what those tests see.

#[path = "../../remind_me_core/src/test_env.rs"]
mod test_env;

use remind_me_core::clients::CLIENTS_FILE_ENV;
use remind_me_core::sync::CLIENT_ENV;
use remind_me_core::tool_profiles::{ALIASES, CORE, GROUPS, MINIMAL, TOOL_PROFILE_ENV, WIKI};
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
    no_registry();
    match profile {
        Some(p) => crate::test_env::set_var(TOOL_PROFILE_ENV, p),
        None => crate::test_env::remove_var(TOOL_PROFILE_ENV),
    }
    let out = body();
    crate::test_env::remove_var(TOOL_PROFILE_ENV);
    out
}

/// Point the client registry at a missing file, so a real
/// `~/.remind-me/clients.json` cannot change what these tests see.
fn no_registry() {
    let missing = std::env::temp_dir().join(format!("rrm_no_clients_{}.json", std::process::id()));
    crate::test_env::set_var(CLIENTS_FILE_ENV, missing);
}

/// Run `body` as client `client` under registry `json`, with
/// `REMIND_ME_TOOL_PROFILE` set to `env_profile`.
fn as_registered<T>(json: &str, client: &str, env_profile: &str, body: impl FnOnce() -> T) -> T {
    let _guard = env_lock();
    let file =
        std::env::temp_dir().join(format!("rrm_clients_{}_{client}.json", std::process::id()));
    std::fs::write(&file, json).unwrap();
    crate::test_env::set_var(CLIENTS_FILE_ENV, &file);
    crate::test_env::set_var(CLIENT_ENV, client);
    crate::test_env::set_var(TOOL_PROFILE_ENV, env_profile);
    let out = body();
    crate::test_env::remove_var(TOOL_PROFILE_ENV);
    crate::test_env::remove_var(CLIENT_ENV);
    no_registry();
    let _ = std::fs::remove_file(&file);
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
    let tools = with_profile(Some("full"), || names(&server()));

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
fn an_unset_profile_lists_core_and_refuses_the_rest() {
    // ADR-0030 step 2: what a client that configures nothing gets.
    let (listed, refused) = with_profile(None, || {
        let s = server();
        (names(&s), call(&s, "remind_me_backup", json!({})))
    });
    assert_eq!(listed, CORE);
    assert_eq!(refused["isError"], true);
    assert!(
        text(&refused).contains("'admin' group"),
        "{}",
        text(&refused)
    );
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
    let unknown: Vec<&str> = with_profile(Some("full"), || {
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
    let result = with_profile(Some("full"), || {
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
    let tools = with_profile(Some("full"), || listed(&server()));
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

/// D68: under `core`, the capture-only caller (the name the remote gate pins)
/// reaches `remind_me_capture_transcript`, nobody else does, and the tool
/// stays unlisted.
#[test]
fn only_the_capture_caller_reaches_the_capture_tool_under_core() {
    use remind_me_mcp::{CALLER_META_KEY, CAPTURE_CLIENT, CAPTURE_TOOL};
    let call = |s: &McpServer, caller: &str, tool: &str| {
        let line = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": tool,
            "arguments": { "session_id": "s1", "transcript": "{\"type\":\"user\"}\n" },
            "_meta": { CALLER_META_KEY: caller }
        }});
        let reply = s.handle_request(&line.to_string()).expect("an answer");
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        !text.contains("not available under")
    };
    with_profile(Some("core"), || {
        let s = server();
        let capture = format!("{CAPTURE_CLIENT}/2");
        assert!(call(&s, &capture, CAPTURE_TOOL));
        assert!(!call(&s, "claude-ai/1.0", CAPTURE_TOOL));
        assert!(!call(&s, &format!("{CAPTURE_CLIENT}-not/2"), CAPTURE_TOOL));
        assert!(!call(&s, &capture, "remind_me_import"));
        let names: Vec<String> = listed(&s)
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        assert!(!names.iter().any(|n| n == CAPTURE_TOOL), "{names:?}");
    });
}

#[test]
fn minimal_lists_exactly_five_tools_and_refuses_core_ones() {
    let (tools, status, list, reminder) = with_profile(Some("minimal"), || {
        let s = server();
        (
            names(&s),
            call(&s, "remind_me_server_status", json!({})),
            call(&s, "remind_me_list", json!({})),
            call(&s, "remind_me_set_reminder", json!({})),
        )
    });
    let mut expected: Vec<&str> = MINIMAL.to_vec();
    expected.sort_by_key(|t| CORE.iter().position(|c| c == t));
    assert_eq!(tools, expected);
    assert_eq!(status["isError"], true);
    let why = text(&status);
    assert!(why.contains("'core' group"), "{why}");
    assert!(
        why.contains(&format!("{TOOL_PROFILE_ENV}=minimal,core")),
        "{why}"
    );
    // An alias works exactly when the tool it merged into is in minimal.
    assert_ne!(list["isError"], true, "{}", text(&list));
    assert_eq!(reminder["isError"], true);
}

#[test]
fn minimal_plus_a_group_adds_it_and_minimal_core_is_core() {
    let with_wiki = with_profile(Some("minimal,wiki"), || names(&server()));
    for tool in MINIMAL.iter().chain(WIKI.iter()) {
        assert!(with_wiki.iter().any(|t| t == tool), "{tool} missing");
    }
    assert!(!with_wiki.iter().any(|t| t == "remind_me_server_status"));
    assert_eq!(
        with_profile(Some("minimal,core"), || names(&server())),
        CORE
    );
    let standard = with_profile(Some("minimal,standard"), || names(&server()));
    assert!(standard.iter().any(|t| t == "remind_me_server_status"));
}

const REGISTRY: &str = r#"{ "hermes-jarvis": { "profile": "minimal", "format": "markdown" } }"#;

#[test]
fn a_registered_profile_beats_the_env_and_its_refusal_names_the_registry() {
    let (tools, refused) = as_registered(REGISTRY, "hermes-jarvis", "full", || {
        let s = server();
        (names(&s), call(&s, "remind_me_update", json!({})))
    });
    assert_eq!(tools.len(), MINIMAL.len(), "{tools:?}");
    let why = text(&refused);
    assert!(why.contains("clients.json"), "{why}");
}

#[test]
fn an_unregistered_client_keeps_the_env_profile() {
    let tools = as_registered(REGISTRY, "someone-else", "core", || names(&server()));
    assert_eq!(tools, CORE);
}

#[test]
fn a_registered_format_is_the_default_when_the_call_sends_none() {
    let registry = r#"{ "json-bot": { "format": "json" } }"#;
    let (implicit, explicit) = as_registered(registry, "json-bot", "core", || {
        let s = server();
        call(&s, "remind_me_add", json!({ "content": "a numbat fact" }));
        (
            call(&s, "remind_me_search", json!({ "query": "numbat" })),
            call(
                &s,
                "remind_me_search",
                json!({ "query": "numbat", "response_format": "markdown" }),
            ),
        )
    });
    // search's own default is Markdown; the registry says JSON for this client.
    let parsed = |r: &Value| serde_json::from_str::<Value>(&text(r)).is_ok();
    assert!(parsed(&implicit), "{}", text(&implicit));
    assert!(!parsed(&explicit), "{}", text(&explicit));
}

#[test]
fn a_registered_format_follows_the_routed_handler() {
    let registry = r#"{ "json-bot": { "format": "json" } }"#;
    let (reference, history) = as_registered(registry, "json-bot", "core", || {
        let s = server();
        let added = call(
            &s,
            "remind_me_add",
            json!({ "content": "see #4242 for the numbat fix" }),
        );
        let id = serde_json::from_str::<Value>(&text(&added)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        (
            // A reference search takes no format: injecting one would refuse it.
            call(&s, "remind_me_search", json!({ "reference": "#4242" })),
            // get's history handler takes one, so it gets the client's default.
            call(
                &s,
                "remind_me_get",
                json!({ "memory_id": id, "history": true }),
            ),
        )
    });
    assert_ne!(
        reference["isError"],
        json!(true),
        "{}",
        text(&reference)
    );
    assert_ne!(
        history["isError"],
        json!(true),
        "{}",
        text(&history)
    );
    assert!(
        serde_json::from_str::<Value>(&text(&history)).is_ok(),
        "{}",
        text(&history)
    );
}

#[test]
fn a_registered_brief_budget_is_the_default_token_budget() {
    let registry = r#"{ "brief-bot": { "brief_budget": 600 } }"#;
    let (implicit, explicit) = as_registered(registry, "brief-bot", "core", || {
        let s = server();
        call(
            &s,
            "remind_me_add",
            json!({ "content": "shipped the wombat parser", "tags": ["wombat"] }),
        );
        call(
            &s,
            "remind_me_entity",
            json!({ "action": "upsert", "name": "wombat" }),
        );
        let brief = |extra: Value| {
            let mut args = json!({ "project": "wombat", "response_format": "json" });
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let out = call(&s, "remind_me_brief", args);
            serde_json::from_str::<Value>(&text(&out)).unwrap_or_else(|_| json!(text(&out)))
        };
        (brief(json!({})), brief(json!({ "token_budget": 900 })))
    });
    assert_eq!(implicit["budget"]["limit"], json!(600), "{implicit}");
    assert_eq!(explicit["budget"]["limit"], json!(900), "{explicit}");
}
