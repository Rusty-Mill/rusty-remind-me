//! The capture-only credential (ADR-0036), driven over real HTTP in both
//! modes: it saves a transcript the way the cloud capture hook does (the
//! legacy session lifecycle: initialize, initialized, tools/call), and is
//! refused for anything else.

mod common;

#[path = "../../remind_me_core/src/test_env.rs"]
mod test_env;

use std::net::SocketAddr;
use std::sync::Arc;

use remind_me_core::Database;
use remind_me_mcp::McpServer;
use serde_json::{json, Value};

const TOKEN: &str = "full-connector-token-0123456789abcdef";
const CAPTURE: &str = "capture-only-token-0123456789abcdef0123";

/// The tool profile and OAuth's state file are read from the environment;
/// every test holds this across its awaits.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `remind_me_capture_transcript` is in the `import` tool group, so a server
/// taking captures runs with it enabled.
async fn lock_env() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = ENV_LOCK.lock().await;
    test_env::set_var(
        remind_me_core::tool_profiles::TOOL_PROFILE_ENV,
        "core,import",
    );
    guard
}

fn spawn(issuer: Option<&str>) -> SocketAddr {
    let mcp = Arc::new(McpServer::new(Database::open_in_memory().unwrap()));
    let router = remind_me_remote::build_router(
        mcp,
        TOKEN.to_string(),
        Some(CAPTURE.to_string()),
        issuer.map(str::to_string),
    )
    .unwrap();
    common::serve(router)
}

async fn spawn_oauth(label: &str) -> (SocketAddr, tokio::sync::MutexGuard<'static, ()>) {
    let guard = lock_env().await;
    let dir = std::env::temp_dir().join(format!("rrm_capture_{label}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    test_env::set_var(
        remind_me_core::remote::REMOTE_OAUTH_STATE_FILE_ENV,
        dir.join("oauth.json"),
    );
    (spawn(Some("http://localhost")), guard)
}

fn headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Content-Type", "application/json".parse().unwrap());
    headers.insert(
        "Accept",
        "application/json, text/event-stream".parse().unwrap(),
    );
    headers.insert(
        "Authorization",
        format!("Bearer {CAPTURE}").parse().unwrap(),
    );
    headers
}

fn payloads(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|event| {
            let data: String = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim)
                .collect();
            serde_json::from_str(&data).ok()
        })
        .collect()
}

async fn post(
    client: &reqwest::Client,
    addr: SocketAddr,
    session: Option<&str>,
    body: Value,
) -> reqwest::Response {
    let mut request = client
        .post(format!("http://{addr}/mcp"))
        .headers(headers())
        .json(&body);
    if let Some(session) = session {
        request = request.header("Mcp-Session-Id", session);
    }
    request.send().await.unwrap()
}

/// Open a session with the capture token, as the hook does.
async fn open_session(client: &reqwest::Client, addr: SocketAddr) -> String {
    let init = post(
        client,
        addr,
        None,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "remind-me-capture-hook", "version": "2" }
        }}),
    )
    .await;
    assert_eq!(init.status(), 200);
    let session = init.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let initialized = post(
        client,
        addr,
        Some(&session),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert!(
        initialized.status().is_success(),
        "{}",
        initialized.status()
    );
    session
}

fn call(id: u64, tool: &str, arguments: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments } })
}

fn transcript_args() -> Value {
    let line = json!({ "type": "user", "message": { "role": "user", "content": "hello from a cloud chat" } });
    json!({ "session_id": "cloud-session-1", "transcript": format!("{line}\n"), "reason": "stop" })
}

async fn assert_capture_saves(addr: SocketAddr) {
    let client = reqwest::Client::new();
    let session = open_session(&client, addr).await;
    let response = post(
        &client,
        addr,
        Some(&session),
        call(2, "remind_me_capture_transcript", transcript_args()),
    )
    .await;
    assert_eq!(response.status(), 200);
    let reply = payloads(&response.text().await.unwrap())
        .into_iter()
        .find(|p| p.get("id") == Some(&json!(2)))
        .expect("the tools/call reply");
    assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
    let outcome: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        stored_client(addr, &outcome["dialog_id"]).await,
        CAPTURE_CLIENT_PREFIX
    );
}

const CAPTURE_CLIENT_PREFIX: &str = "remind-me-capture-token";

/// The `client` the stored memory `id` was written by, read with the full
/// connector token, cut at its version.
async fn stored_client(addr: SocketAddr, id: &Value) -> String {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/mcp/{TOKEN}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", "remind_me_get")
        .json(
            &json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {
                "name": "remind_me_get",
                "arguments": { "memory_id": id, "response_format": "json" },
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }}),
        )
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();
    let reply = payloads(&body)
        .into_iter()
        .find(|p| p.get("id") == Some(&json!(9)))
        .unwrap_or_else(|| panic!("no remind_me_get reply in {body}"));
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    let memory: Value = serde_json::from_str(text).unwrap_or_else(|_| panic!("{reply}"));
    let client = memory["client"]
        .as_str()
        .unwrap_or_else(|| panic!("{memory}"));
    client.split('/').next().unwrap().to_string()
}

async fn assert_everything_else_is_forbidden(addr: SocketAddr) {
    let client = reqwest::Client::new();
    let session = open_session(&client, addr).await;
    for body in [
        call(3, "remind_me_search", json!({ "query": "hello" })),
        call(4, "remind_me_delete", json!({ "memory_id": "anything" })),
        json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" }),
        json!([call(6, "remind_me_capture_transcript", transcript_args())]),
    ] {
        let response = post(&client, addr, Some(&session), body.clone()).await;
        assert_eq!(response.status(), 403, "{body}");
        let refusal: Value = response.json().await.unwrap();
        assert_eq!(refusal, json!({ "error": "Forbidden" }));
    }
    let stream = client
        .get(format!("http://{addr}/mcp"))
        .headers(headers())
        .header("Mcp-Session-Id", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 403);
}

#[tokio::test]
async fn legacy_mode_capture_token_saves_a_transcript() {
    let _guard = lock_env().await;
    assert_capture_saves(spawn(None)).await;
}

#[tokio::test]
async fn legacy_mode_capture_token_is_refused_for_anything_else() {
    let _guard = lock_env().await;
    assert_everything_else_is_forbidden(spawn(None)).await;
}

#[tokio::test]
async fn the_capture_token_has_no_secret_path_form() {
    let _guard = lock_env().await;
    let addr = spawn(None);
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/mcp/{CAPTURE}"))
        .headers(headers())
        .json(&call(1, "remind_me_capture_transcript", transcript_args()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn oauth_mode_capture_token_saves_a_transcript() {
    let (addr, _guard) = spawn_oauth("saves").await;
    assert_capture_saves(addr).await;
}

#[tokio::test]
async fn oauth_mode_capture_token_is_refused_for_anything_else() {
    let (addr, _guard) = spawn_oauth("refused").await;
    assert_everything_else_is_forbidden(addr).await;
}

#[tokio::test]
async fn without_a_capture_token_configured_it_is_just_a_wrong_bearer() {
    let _guard = lock_env().await;
    let mcp = Arc::new(McpServer::new(Database::open_in_memory().unwrap()));
    let router = remind_me_remote::build_router(mcp, TOKEN.to_string(), None, None).unwrap();
    let addr = common::serve(router);
    let response = post(
        &reqwest::Client::new(),
        addr,
        None,
        call(1, "remind_me_capture_transcript", transcript_args()),
    )
    .await;
    assert_eq!(response.status(), 401);
}
