//! Integration tests driving the real `rusty_axum` server over HTTP.
//!
//! These exercise `remind_me_remote::build_router` exactly as production
//! uses it (default `StreamableHttpServerConfig`, so real session-managed
//! SSE, not the `with_json_response(true)` shortcut some of rmcp's own
//! tests use) -- what a real claude.ai connector would actually receive.
//! See `crate::handler`'s module doc and this crate's ADR for why unit
//! tests alone can't cover `ServerHandler` dispatch (its `RequestContext`
//! has no public constructor outside `rmcp`): this file is where that
//! coverage lives instead.

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use remind_me_core::daemon::endpoint::Endpoint;
use remind_me_core::daemon::ops::{Op, OpReply};
use remind_me_core::daemon::server::{Daemon, Service, Start};
use remind_me_core::Database;
use remind_me_mcp::daemon_proxy::DaemonProxy;
use remind_me_mcp::{Handler, McpServer};
use serde_json::{json, Value};

const TOKEN: &str = "test-connector-token";

/// Spin up `build_router` on an ephemeral loopback port and return its
/// address plus a cancellation-free background task -- the listener and
/// server both drop, and the OS reclaims the port, when the test ends.
async fn spawn_server() -> SocketAddr {
    let db = Database::open_in_memory().unwrap();
    let mcp = Arc::new(McpServer::new(db));
    let router = remind_me_remote::build_router(mcp, TOKEN.to_string(), None).unwrap();
    common::serve(router)
}

/// The daemon's side of the production setup: an MCP server over the store,
/// answering lines that arrive on daemon connections.
struct DaemonStore(McpServer);

impl Service for DaemonStore {
    fn mcp(&self, line: &str) -> Option<Value> {
        self.0.handle_line(line)
    }

    fn op(&self, _: &Op) -> OpReply {
        OpReply::Err("not served by this fixture".to_string())
    }

    fn http(&self, _: &mut std::net::TcpStream) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

/// [`spawn_server`] as production runs it: the connector's handler is a
/// [`DaemonProxy`] relaying every caller's lines over one connection to a
/// daemon that owns the store, so writes run on the daemon's thread.
async fn spawn_daemon_backed_server() -> SocketAddr {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rrm_remote_daemon_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let endpoint = Endpoint::for_db(&dir.join("memory.db"));
    let Start::Started(locked) = Daemon::start(endpoint.clone()).unwrap() else {
        panic!("a fresh endpoint must start");
    };
    let daemon = locked.listen().unwrap();
    let store = DaemonStore(McpServer::new(Database::open_in_memory().unwrap()));
    // Serves for the rest of the test binary's life, like `common::serve`.
    std::thread::spawn(move || daemon.serve(Arc::new(store)));
    // The daemon is already running, so the executable is never started.
    let proxy = DaemonProxy::connect(endpoint, PathBuf::from("unused")).unwrap();
    let router = remind_me_remote::build_router(Arc::new(proxy), TOKEN.to_string(), None).unwrap();
    common::serve(router)
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
async fn a_body_over_four_mib_is_refused_with_413() {
    use std::io::{Read, Write};

    let addr = spawn_server().await;
    // Only the head: the server refuses on the declared length, before any
    // of the body is sent.
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        4 * 1024 * 1024 + 1
    );
    let reply = tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        let mut reply = Vec::new();
        let _ = stream.read_to_end(&mut reply);
        String::from_utf8_lossy(&reply).into_owned()
    })
    .await
    .unwrap();
    assert!(reply.starts_with("HTTP/1.1 413"), "{reply}");
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

/// Open a legacy session as `client_name`, returning its id and the
/// `initialize` result.
async fn open_session(
    client: &reqwest::Client,
    addr: SocketAddr,
    client_name: &str,
) -> (String, Value) {
    let init = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(mcp_headers())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": client_name, "version": "1.0" }
            }
        }))
        .send()
        .await
        .unwrap();
    let session_id = init.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let payloads = sse_json_payloads(&init.text().await.unwrap());
    let result = payloads
        .into_iter()
        .find(|p| p.get("id") == Some(&json!(1)))
        .expect("the initialize response must be among the SSE payloads");
    client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(mcp_headers())
        .header("Mcp-Session-Id", &session_id)
        .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .send()
        .await
        .unwrap();
    (session_id, result["result"].clone())
}

/// Add `content` over `session_id` and return the stored memory.
async fn add_over(
    client: &reqwest::Client,
    addr: SocketAddr,
    session_id: &str,
    content: &str,
) -> Value {
    let call = client
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .headers(mcp_headers())
        .header("Mcp-Session-Id", session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "remind_me_add", "arguments": { "content": content } }
        }))
        .send()
        .await
        .unwrap();
    let payloads = sse_json_payloads(&call.text().await.unwrap());
    let result = payloads
        .iter()
        .find(|p| p.get("id") == Some(&json!(2)))
        .expect("the tools/call response must be among the SSE payloads");
    let text = result["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn initialize_tells_a_remote_client_how_to_use_the_memory() {
    let addr = spawn_server().await;
    let (_, init) = open_session(&reqwest::Client::new(), addr, "claude-ai").await;
    assert_eq!(init["instructions"], remind_me_mcp::SERVER_INSTRUCTIONS);
}

/// One server, two connected clients taking turns: each memory names the
/// client that wrote it, not "unknown" and not whichever shook hands last.
async fn assert_alternating_callers_stamp_their_own_writes(addr: SocketAddr) {
    let client = reqwest::Client::new();
    let (claude, _) = open_session(&client, addr, "claude-ai").await;
    let (chatgpt, _) = open_session(&client, addr, "chatgpt").await;

    let first = add_over(&client, addr, &claude, "written by claude.ai").await;
    let second = add_over(&client, addr, &chatgpt, "written by chatgpt").await;
    let third = add_over(&client, addr, &claude, "claude.ai again").await;

    assert_eq!(first["client"], "claude-ai/1.0", "{first}");
    assert_eq!(second["client"], "chatgpt/1.0", "{second}");
    assert_eq!(third["client"], "claude-ai/1.0", "{third}");
}

#[tokio::test]
async fn each_remote_caller_stamps_its_own_writes() {
    assert_alternating_callers_stamp_their_own_writes(spawn_server().await).await;
}

#[tokio::test]
async fn each_remote_caller_stamps_its_own_writes_through_the_daemon() {
    assert_alternating_callers_stamp_their_own_writes(spawn_daemon_backed_server().await).await;
}

#[tokio::test]
async fn concurrent_remote_callers_through_the_daemon_never_swap_names() {
    let addr = spawn_daemon_backed_server().await;
    let client = reqwest::Client::new();
    let (claude, _) = open_session(&client, addr, "claude-ai").await;
    let (chatgpt, _) = open_session(&client, addr, "chatgpt").await;

    for round in 0..8 {
        let (from_claude, from_chatgpt) = (
            format!("claude.ai round {round}"),
            format!("chatgpt round {round}"),
        );
        let (a, b) = tokio::join!(
            add_over(&client, addr, &claude, &from_claude),
            add_over(&client, addr, &chatgpt, &from_chatgpt),
        );
        assert_eq!(a["client"], "claude-ai/1.0", "{a}");
        assert_eq!(b["client"], "chatgpt/1.0", "{b}");
    }
}

#[tokio::test]
async fn a_nameless_remote_caller_through_the_daemon_gets_the_configured_client() {
    // A caller with a blank name named nobody: its write falls to the
    // configured client, never to a named caller that wrote just before it.
    let addr = spawn_daemon_backed_server().await;
    let client = reqwest::Client::new();
    let (named, _) = open_session(&client, addr, "claude-ai").await;
    let (nameless, _) = open_session(&client, addr, " ").await;

    add_over(&client, addr, &named, "named first").await;
    let memory = add_over(&client, addr, &nameless, "nameless after").await;

    let configured =
        std::env::var(remind_me_core::sync::CLIENT_ENV).unwrap_or_else(|_| "unknown".into());
    assert_eq!(memory["client"], configured.as_str(), "{memory}");
}
