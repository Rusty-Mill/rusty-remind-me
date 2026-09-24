//! Integration tests driving the real axum/rmcp server over HTTP.
//!
//! These exercise `remind_me_remote::build_router` exactly as production
//! uses it (default `StreamableHttpServerConfig`, so real session-managed
//! SSE, not the `with_json_response(true)` shortcut some of rmcp's own
//! tests use) -- what a real claude.ai connector would actually receive.
//! See `crate::handler`'s module doc and this crate's ADR for why unit
//! tests alone can't cover `ServerHandler` dispatch (its `RequestContext`
//! has no public constructor outside `rmcp`): this file is where that
//! coverage lives instead.

use std::net::SocketAddr;
use std::sync::Arc;

use remind_me_core::Database;
use remind_me_mcp::McpServer;
use serde_json::{json, Value};

const TOKEN: &str = "test-connector-token";

/// Spin up `build_router` on an ephemeral loopback port and return its
/// address plus a cancellation-free background task -- the listener and
/// server both drop, and the OS reclaims the port, when the test ends.
async fn spawn_server() -> SocketAddr {
    let db = Database::open_in_memory().unwrap();
    let mcp = Arc::new(McpServer::new(db));
    let router = remind_me_remote::build_router(mcp, TOKEN.to_string(), None).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

/// Extract every JSON payload carried by an SSE response body's `data:`
/// fields -- rmcp's real wire format, not a JSON-RPC body directly. The
/// first event is typically an empty-data retry/priming event; that's
/// filtered out here rather than asserted on, since its presence isn't
/// something this crate controls.
fn sse_json_payloads(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|event| {
            let data: String = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                None
            } else {
                serde_json::from_str(&data).ok()
            }
        })
        .collect()
}

/// Every SSE event in a response body as `(id, parsed data)` pairs, in
/// delivery order -- unlike `sse_json_payloads`, this keeps the priming
/// event (empty `data:`) rather than filtering it out, and keeps its `id:`
/// line. That id is exactly what `InProcessEventStore` assigns once an
/// event store is attached (see `server::build_router`), and exactly what a
/// client resuming via `Last-Event-Id` after that priming event would send.
fn sse_events(body: &str) -> Vec<(Option<String>, Option<Value>)> {
    body.split("\n\n")
        .filter(|event| !event.trim().is_empty())
        .map(|event| {
            let mut id = None;
            let mut data_lines = Vec::new();
            for line in event.lines() {
                if let Some(rest) = line.strip_prefix("id:") {
                    id = Some(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data_lines.push(rest.trim());
                }
            }
            let data = data_lines.join("\n");
            let parsed = if data.is_empty() {
                None
            } else {
                serde_json::from_str(&data).ok()
            };
            (id, parsed)
        })
        .collect()
}

/// The per-request `_meta` a 2026-07-28 client attaches to every
/// session-free (discover-lifecycle) request. The spec requires both keys,
/// and rmcp enforces it from 3.1 on (a missing one is `-32602 Invalid
/// params`); 3.0.1, which this crate used before joining the rusty_mill
/// workspace, did not check.
fn discover_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

fn mcp_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Content-Type", "application/json".parse().unwrap());
    headers.insert(
        "Accept",
        "application/json, text/event-stream".parse().unwrap(),
    );
    headers
}

#[tokio::test]
async fn health_probe_is_unauthenticated_and_reveals_no_data() {
    let addr = spawn_server().await;
    let response = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body, json!({ "status": "ok" }));
}

#[tokio::test]
async fn mcp_without_any_credential_is_rejected_with_401() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/mcp"))
        .headers(mcp_headers())
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body, json!({ "error": "Unauthorized" }));
}

#[tokio::test]
async fn mcp_with_the_wrong_bearer_token_is_rejected_with_401() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/mcp"))
        .headers(mcp_headers())
        .header("Authorization", "Bearer not-the-real-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn a_wrong_secret_path_token_404s_identically_to_an_unrelated_path() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    let wrong_token = client
        .get(format!("http://{addr}/mcp/not-the-real-token"))
        .send()
        .await
        .unwrap();
    let unrelated = client
        .get(format!("http://{addr}/totally/unrelated"))
        .send()
        .await
        .unwrap();

    assert_eq!(wrong_token.status(), 404);
    assert_eq!(unrelated.status(), 404);
    // Same status AND same body: a probe cannot tell "wrong token" apart
    // from "this path was never a thing".
    assert_eq!(
        wrong_token.text().await.unwrap(),
        unrelated.text().await.unwrap()
    );
}

#[tokio::test]
async fn an_empty_secret_path_segment_404s_rather_than_matching_the_empty_string() {
    let addr = spawn_server().await;
    let response = reqwest::get(format!("http://{addr}/mcp/")).await.unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn secret_path_initialize_negotiates_a_real_session_managed_sse_stream() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(mcp_headers())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "integration-test", "version": "0" }
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "the secret-path form must reach the real rmcp session transport, \
         not a plain JSON responder"
    );
    assert!(
        response.headers().contains_key("mcp-session-id"),
        "a session-managed (legacy_session_mode default) server must hand \
         back a session id on initialize"
    );

    let body = response.text().await.unwrap();
    let payloads = sse_json_payloads(&body);
    let init_result = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(1)))
        .expect("the initialize response must be among the SSE payloads");
    assert_eq!(
        init_result["result"]["serverInfo"]["name"],
        "rusty_remind_me"
    );
    assert!(init_result["result"]["capabilities"]["tools"].is_object());
}

#[tokio::test]
async fn bearer_auth_reuses_a_session_the_secret_path_opened_and_round_trips_a_real_tool_call() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    // Open the session via the secret-path form (no Authorization header).
    let init = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(mcp_headers())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "integration-test", "version": "0" }
            }
        }))
        .send()
        .await
        .unwrap();
    let session_id = init
        .headers()
        .get("mcp-session-id")
        .expect("initialize must return a session id")
        .to_str()
        .unwrap()
        .to_string();
    let _ = init.text().await.unwrap();

    // Required before further requests on a freshly initialized session.
    let initialized_ack = client
        .post(format!("http://{addr}/mcp"))
        .headers(mcp_headers())
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Mcp-Session-Id", &session_id)
        .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .send()
        .await
        .unwrap();
    assert!(initialized_ack.status().is_success());

    // Now call a real tool, over the bearer form, reusing the session the
    // secret-path form opened -- proving the two ways in are interchangeable
    // within one session, exactly as the reference documents.
    let call = client
        .post(format!("http://{addr}/mcp"))
        .headers(mcp_headers())
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Mcp-Session-Id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "remind_me_add",
                "arguments": { "content": "written through the remote transport" }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(call.status(), 200);
    let body = call.text().await.unwrap();
    let payloads = sse_json_payloads(&body);
    let result = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(2)))
        .expect("the tools/call response must be among the SSE payloads");
    let text = result["result"]["content"][0]["text"]
        .as_str()
        .expect("call_tool result content must carry the stored memory as text");
    assert!(
        text.contains("written through the remote transport"),
        "expected the round-tripped memory content in the tool result, got: {text}"
    );
    assert_ne!(result["result"]["isError"], json!(true));
}

#[tokio::test]
async fn a_2026_07_28_client_calls_a_tool_in_one_post_with_no_session_at_all() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    // No prior `initialize`, no `Mcp-Session-Id` -- SEP-2567's discover
    // lifecycle dispatches per request. `MCP-Protocol-Version: 2026-07-28`
    // is what routes `handle_post` there instead of the legacy session
    // path; `Mcp-Method` is the SEP-2243 standard header that version also
    // requires (`tools/list` has no `Mcp-Name` counterpart -- only
    // name-bearing methods like `tools/call` do).
    let mut headers = mcp_headers();
    headers.insert("MCP-Protocol-Version", "2026-07-28".parse().unwrap());
    headers.insert("Mcp-Method", "tools/list".parse().unwrap());

    let response = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(headers)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": { "_meta": discover_meta() }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert!(
        !response.headers().contains_key("mcp-session-id"),
        "the discover lifecycle is session-free -- a 2026-07-28 request \
         must not mint a legacy session"
    );

    let body = response.text().await.unwrap();
    let payloads = sse_json_payloads(&body);
    let result = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(1)))
        .expect("the tools/list response must be among the SSE payloads");
    let tools = result["result"]["tools"]
        .as_array()
        .expect("tools/list result must carry a tools array");
    assert!(
        tools.iter().any(|tool| tool["name"] == "remind_me_add"),
        "expected the real tool list dispatched through RemindMeHandler, got: {tools:?}"
    );
}

#[tokio::test]
async fn a_2025_11_25_client_still_uses_the_legacy_session_lifecycle() {
    // 2025-11-25 predates SEP-2567 (that lands at 2026-07-28) -- confirms
    // `is_legacy_request`'s version cutoff, not just the two edges already
    // covered by the default (2024-11-05, via the other tests) and the new
    // 2026-07-28 discover lifecycle above.
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(mcp_headers())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "integration-test", "version": "0" }
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert!(
        response.headers().contains_key("mcp-session-id"),
        "2025-11-25 is still legacy-lifecycle and must get a session id"
    );
}

#[tokio::test]
async fn a_2026_07_28_client_calls_a_mutating_tool_in_one_post_with_no_session_at_all() {
    // The one `tools/call` coverage this file already had
    // (`bearer_auth_reuses_a_session_...`) runs entirely over the legacy
    // session lifecycle. This is the discover-lifecycle counterpart:
    // `tools/call` also carries an SEP-2243 `Mcp-Name` header (`tools/call`
    // is in `NAME_FROM_NAME`, sourced from `params.name`), unlike
    // `tools/list` above.
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    let mut headers = mcp_headers();
    headers.insert("MCP-Protocol-Version", "2026-07-28".parse().unwrap());
    headers.insert("Mcp-Method", "tools/call".parse().unwrap());
    headers.insert("Mcp-Name", "remind_me_add".parse().unwrap());

    let response = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(headers)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "remind_me_add",
                "arguments": { "content": "written through the discover lifecycle" },
                "_meta": discover_meta()
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key("mcp-session-id"));

    let body = response.text().await.unwrap();
    let payloads = sse_json_payloads(&body);
    let result = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(1)))
        .expect("the tools/call response must be among the SSE payloads");
    let text = result["result"]["content"][0]["text"]
        .as_str()
        .expect("call_tool result content must carry the stored memory as text");
    assert!(
        text.contains("written through the discover lifecycle"),
        "expected the round-tripped memory content, got: {text}"
    );
    assert_ne!(result["result"]["isError"], json!(true));
}

#[tokio::test]
async fn a_2026_07_28_client_reads_the_stats_resource_with_no_session() {
    // `resources/read`'s `Mcp-Name` is sourced from `params.uri` (it's in
    // `NAME_FROM_URI`, not `NAME_FROM_NAME`) -- the one SEP-2243 header
    // shape neither of the other two new tests exercises.
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    let mut headers = mcp_headers();
    headers.insert("MCP-Protocol-Version", "2026-07-28".parse().unwrap());
    headers.insert("Mcp-Method", "resources/read".parse().unwrap());
    headers.insert("Mcp-Name", "memory://stats".parse().unwrap());

    let response = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(headers)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/read",
            "params": { "uri": "memory://stats", "_meta": discover_meta() }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key("mcp-session-id"));

    let body = response.text().await.unwrap();
    let payloads = sse_json_payloads(&body);
    let result = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(1)))
        .expect("the resources/read response must be among the SSE payloads");
    assert_eq!(result["result"]["contents"][0]["uri"], "memory://stats");
}

#[tokio::test]
async fn a_2026_07_28_client_lists_prompts_with_no_session() {
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    let mut headers = mcp_headers();
    headers.insert("MCP-Protocol-Version", "2026-07-28".parse().unwrap());
    headers.insert("Mcp-Method", "prompts/list".parse().unwrap());

    let response = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(headers)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "prompts/list",
            "params": { "_meta": discover_meta() }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key("mcp-session-id"));

    let body = response.text().await.unwrap();
    let payloads = sse_json_payloads(&body);
    let result = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(1)))
        .expect("the prompts/list response must be among the SSE payloads");
    let prompts = result["result"]["prompts"]
        .as_array()
        .expect("prompts/list result must carry a prompts array");
    assert!(
        prompts.iter().any(|p| p["name"] == "recall_context"),
        "expected the real prompt list dispatched through RemindMeHandler, got: {prompts:?}"
    );
}

#[tokio::test]
async fn a_2026_07_28_client_resumes_a_dropped_response_stream_via_last_event_id() {
    // The actual mechanism `InProcessEventStore` exists for: before it was
    // wired into `build_router`, a discover-lifecycle POST's SSE response
    // was never persisted anywhere, so a client that dropped its connection
    // before reading the response lost it for good. Now every event on that
    // response -- including the leading empty-data priming event -- gets a
    // real, replayable id (`tower.rs`'s `persisted_stateless_stream`, which
    // only runs when `session_manager.event_store()` is `Some`).
    let addr = spawn_server().await;
    let client = reqwest::Client::new();

    let mut headers = mcp_headers();
    headers.insert("MCP-Protocol-Version", "2026-07-28".parse().unwrap());
    headers.insert("Mcp-Method", "tools/list".parse().unwrap());

    let response = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(headers)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": { "_meta": discover_meta() }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let body = response.text().await.unwrap();
    let events = sse_events(&body);
    let (priming_id, priming_data) = events
        .first()
        .expect("expected at least a priming event in the response");
    assert!(
        priming_data.is_none(),
        "expected an empty-data priming event first, got {events:?}"
    );
    let priming_id = priming_id
        .clone()
        .expect("the priming event must carry a real event id once an EventStore is attached");

    // Simulate the client dropping before it ever saw the real response:
    // reconnect with `Last-Event-Id` pointing at the priming event it did
    // see -- no `Mcp-Session-Id` at all, since this is the session-free
    // stateless-replay path (`tower.rs::handle_get`'s non-legacy branch).
    let mut resume_headers = reqwest::header::HeaderMap::new();
    resume_headers.insert("Accept", "text/event-stream".parse().unwrap());
    resume_headers.insert("MCP-Protocol-Version", "2026-07-28".parse().unwrap());
    resume_headers.insert("Last-Event-Id", priming_id.parse().unwrap());

    let resumed = client
        .get(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(resume_headers)
        .send()
        .await
        .unwrap();
    let resumed_status = resumed.status();
    let resumed_body = resumed.text().await.unwrap();
    assert_eq!(resumed_status, 200, "{resumed_body}");

    let resumed_events = sse_events(&resumed_body);
    let result = resumed_events
        .iter()
        .find_map(|(_, data)| data.clone())
        .expect("the resumed stream must replay the tools/list response the client missed");
    assert!(
        result["result"]["tools"].as_array().is_some(),
        "expected the real tools/list result to be replayed, got: {result}"
    );
}
