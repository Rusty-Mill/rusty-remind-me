//! Every tool's default output format, and the per-call override (#224).
//!
//! # Why this drives the server rather than the dispatch arms
//!
//! The bug this file exists for was not a missing branch that a unit test would
//! have caught. `MemorySearchInput` and `MemoryListInput` *already* carried
//! `response_format`, already defaulting to Markdown exactly as the reference
//! does — the value was parsed correctly and then discarded when the arm
//! serialized JSON regardless. Reading either the struct or the input model
//! showed a correct default. Only the returned text showed the truth.
//!
//! So everything here goes through `handle_request` and asserts on the text a
//! caller actually receives.
//!
//! # Why the expected defaults are per-tool
//!
//! There is no single right default, and the temptation to impose one is what
//! #225 got wrong before being closed as not-a-bug. Two populations:
//!
//! - Tools mirroring a `remind_me` input model take **that model's** default.
//!   The reference sets MARKDOWN on seven of its eight and JSON on
//!   `VitalityReportInput`.
//! - The twelve tools from #211 take **JSON**, because the reference has no
//!   `response_format` for them at all — it returns Markdown and offers no
//!   JSON, so the parameter is a pure addition here and JSON keeps this port's
//!   existing callers working (#206).

#[path = "../../remind_me_core/src/test_env.rs"]
mod test_env;

use remind_me_mcp::McpServer;
use serde_json::json;

/// A server whose wiki is a directory this test owns.
///
/// `McpServer::new` uses `Wiki::from_env()`, and `list_pages` *reconciles from
/// the filesystem* before reading the index — so an in-memory database is not
/// isolation. A server built with `new` picks up whatever pages happen to be in
/// the shared wiki root, which made the empty-wiki assertion below pass or fail
/// depending on what else had ever run on the machine.
fn server() -> McpServer {
    let dir = std::env::temp_dir().join(format!(
        "rrm-fmt-test-{}-{:?}",
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

fn call(s: &McpServer, tool: &str, args: serde_json::Value) -> String {
    let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                     "params":{"name":tool,"arguments":args}})
    .to_string();
    let r = s.handle_request(&req).expect("the server should answer");
    r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// JSON responses are serialized objects or arrays; Markdown never is.
fn is_json(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('{') || t.starts_with('[')
}

fn seed(s: &McpServer) {
    call(
        s,
        "remind_me_add",
        json!({"content": "alpha beta gamma", "category": "general"}),
    );
}

/// The four tools that mirror a reference model, with the reference's own
/// default for each. Deliberately written as reference facts with a file
/// reference, so changing one means restating what `remind_me` does.
const MIRRORED: [(&str, &str, bool); 4] = [
    // (tool, reference model & line, reference default is markdown)
    ("remind_me_search", "MemorySearchInput, models.py:199", true),
    ("remind_me_list", "MemoryListInput, models.py:365", true),
    ("remind_me_wiki_list", "WikiListInput, models.py:1547", true),
    (
        "remind_me_vitality_report",
        "VitalityReportInput, models.py:1653",
        false,
    ),
];

fn args_for(tool: &str) -> serde_json::Value {
    match tool {
        "remind_me_search" => json!({"query": "alpha"}),
        _ => json!({}),
    }
}

// ---------------------------------------------------------------------------
// Defaults match the reference, per tool
// ---------------------------------------------------------------------------

#[test]
fn each_mirrored_tool_defaults_to_what_the_reference_defaults_to() {
    let s = server();
    seed(&s);
    for (tool, reference, markdown_default) in MIRRORED {
        let text = call(&s, tool, args_for(tool));
        assert_eq!(
            !is_json(&text),
            markdown_default,
            "{tool} default disagrees with {reference}; got: {}",
            text.chars().take(120).collect::<String>()
        );
    }
}

#[test]
fn an_explicit_format_is_honoured_in_both_directions() {
    // The precise shape of the bug: the parameter deserialized fine and was
    // then dropped, so a markdown request returned a *successful* JSON body.
    // Asserting both directions means neither can be satisfied by hardcoding.
    let s = server();
    seed(&s);
    for (tool, _, _) in MIRRORED {
        let mut as_json = args_for(tool);
        as_json["response_format"] = json!("json");
        assert!(
            is_json(&call(&s, tool, as_json)),
            "{tool} ignored an explicit json request"
        );

        let mut as_md = args_for(tool);
        as_md["response_format"] = json!("markdown");
        assert!(
            !is_json(&call(&s, tool, as_md)),
            "{tool} ignored an explicit markdown request"
        );
    }
}

#[test]
fn an_unknown_format_falls_back_to_the_tools_default_rather_than_erroring() {
    let s = server();
    seed(&s);
    for (tool, _, markdown_default) in MIRRORED {
        let mut args = args_for(tool);
        args["response_format"] = json!("yaml");
        let text = call(&s, tool, args);
        assert!(!text.is_empty(), "{tool} returned nothing for a bad format");
        assert_eq!(
            !is_json(&text),
            markdown_default,
            "{tool} should fall back to its own default, not a global one"
        );
    }
}

#[test]
fn the_four_schemas_advertise_the_parameter() {
    // Previously none did — a caller reading `tools/list` had no way to learn
    // the parameter existed. Extracted per tool from the listing rather than
    // grepped from source: an unbounded grep runs into the *next* tool's
    // properties and reports the wrong answer, which is a mistake this issue
    // already made once.
    let s = server();
    let listed = s
        .handle_request(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string())
        .unwrap();
    let tools = listed["result"]["tools"].as_array().unwrap();
    for (tool, _, markdown_default) in MIRRORED {
        let entry = tools
            .iter()
            .find(|t| t["name"] == tool)
            .unwrap_or_else(|| panic!("{tool} missing from tools/list"));
        let prop = &entry["inputSchema"]["properties"]["response_format"];
        assert!(!prop.is_null(), "{tool} does not advertise response_format");
        // The advertised default has to agree with the behaviour above, or the
        // schema documents a tool that does not exist.
        assert_eq!(
            prop["default"],
            if markdown_default { "markdown" } else { "json" },
            "{tool} advertises a default it does not honour"
        );
    }
}

// ---------------------------------------------------------------------------
// The renderings themselves
// ---------------------------------------------------------------------------

#[test]
fn an_empty_wiki_says_how_to_populate_it() {
    // Verbatim from the reference (tools/wiki.py:129). An empty index is far
    // more often "nothing synthesised yet" than "nothing to synthesise", and
    // the message is what tells them apart.
    let s = server();
    let text = call(&s, "remind_me_wiki_list", json!({}));
    assert!(text.contains("The wiki is empty"), "got: {text}");
    assert!(text.contains("remind_me_wiki_compile"), "got: {text}");
}

#[test]
fn search_markdown_carries_the_budget_envelope() {
    // The trimming envelope (#200) is the part a caller cannot recover from
    // the rendered memories: a response cut in half otherwise looks complete.
    let s = server();
    seed(&s);
    let text = call(
        &s,
        "remind_me_search",
        json!({"query": "alpha", "response_format": "markdown"}),
    );
    assert!(text.contains("results"), "no result count in: {text}");
    assert!(text.contains("tokens"), "no budget line in: {text}");
    assert!(text.contains("alpha beta gamma"), "no content in: {text}");
}

#[test]
fn vitality_markdown_formats_the_average_to_two_places() {
    // `:.2f` in the reference. Rust's default float formatting would print
    // something like 0.8333333333333334 here.
    let s = server();
    seed(&s);
    let text = call(
        &s,
        "remind_me_vitality_report",
        json!({"response_format": "markdown"}),
    );
    assert!(text.contains("## Vault Vitality Report"), "got: {text}");
    let avg = text
        .lines()
        .find(|l| l.starts_with("**Average vitality:**"))
        .unwrap_or_else(|| panic!("no average line in: {text}"));
    let value = avg.trim_start_matches("**Average vitality:**").trim();
    let decimals = value.split_once('.').map(|(_, d)| d.len()).unwrap_or(0);
    assert_eq!(decimals, 2, "expected two decimal places, got {value:?}");
}

// ---------------------------------------------------------------------------
// The global default switch (#226)
//
// `REMIND_ME_DEFAULT_RESPONSE_FORMAT` is process-global, so these run one at a
// time and always restore what they found. Cargo runs a binary's tests on
// several threads; without the lock they would pass or fail on scheduling.
// ---------------------------------------------------------------------------

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_default_format<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var(remind_me_mcp::DEFAULT_FORMAT_ENV).ok();
    match value {
        Some(v) => crate::test_env::set_var(remind_me_mcp::DEFAULT_FORMAT_ENV, v),
        None => crate::test_env::remove_var(remind_me_mcp::DEFAULT_FORMAT_ENV),
    }
    let out = f();
    match previous {
        Some(v) => crate::test_env::set_var(remind_me_mcp::DEFAULT_FORMAT_ENV, v),
        None => crate::test_env::remove_var(remind_me_mcp::DEFAULT_FORMAT_ENV),
    }
    out
}

/// Three of the twelve tools from #211 — the population this switch moves.
const ADDITIVE: [&str; 3] = [
    "remind_me_check_update",
    "remind_me_reindex",
    "remind_me_server_status",
];

#[test]
fn unset_leaves_every_byte_as_it_was() {
    // The promise that makes this additive rather than a breaking change.
    with_default_format(None, || {
        let s = server();
        for tool in ADDITIVE {
            assert!(
                is_json(&call(&s, tool, json!({}))),
                "{tool} should still default to json when the variable is unset"
            );
        }
    });
}

#[test]
fn markdown_moves_the_additive_tools() {
    with_default_format(Some("markdown"), || {
        let s = server();
        for tool in ADDITIVE {
            let text = call(&s, tool, json!({}));
            assert!(
                !is_json(&text),
                "{tool} should render markdown under the switch; got: {}",
                text.chars().take(80).collect::<String>()
            );
        }
    });
}

#[test]
fn the_switch_does_not_touch_reference_mandated_defaults() {
    // The subtle half. `vitality_report` mirrors `VitalityReportInput`, which
    // the reference defaults to JSON. Making it render Markdown because
    // somebody asked for "markdown defaults" would move this port *away* from
    // the reference — the opposite of what the switch is for.
    with_default_format(Some("markdown"), || {
        let s = server();
        assert!(
            is_json(&call(&s, "remind_me_vitality_report", json!({}))),
            "vitality_report must keep the reference's JSON default"
        );
        // And the Markdown-defaulting mirrors stay Markdown, unchanged.
        seed(&s);
        assert!(!is_json(&call(&s, "remind_me_list", json!({}))));
    });
}

#[test]
fn an_explicit_argument_still_beats_the_switch_in_both_directions() {
    with_default_format(Some("markdown"), || {
        let s = server();
        assert!(
            is_json(&call(
                &s,
                "remind_me_server_status",
                json!({"response_format": "json"})
            )),
            "an explicit json request must win over a markdown default"
        );
    });
    with_default_format(Some("json"), || {
        let s = server();
        assert!(
            !is_json(&call(
                &s,
                "remind_me_server_status",
                json!({"response_format": "markdown"})
            )),
            "an explicit markdown request must win over a json default"
        );
    });
}

#[test]
fn a_typo_selects_json_rather_than_markdown() {
    // Failing toward the documented default. A misspelling that silently
    // enabled Markdown would change output for someone who thought they had
    // configured nothing.
    for bogus in ["markdwon", "md", "MARKDOWN ", "", "  "] {
        with_default_format(Some(bogus), || {
            let s = server();
            let text = call(&s, "remind_me_server_status", json!({}));
            let markdown_expected = bogus.trim().eq_ignore_ascii_case("markdown");
            assert_eq!(
                !is_json(&text),
                markdown_expected,
                "{bogus:?} resolved the wrong way"
            );
        });
    }
}
