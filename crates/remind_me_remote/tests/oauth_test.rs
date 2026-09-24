//! Integration tests driving the OAuth authorization server (FT-07, `#86`)
//! over real HTTP, the same way `http_test.rs` drives the FT-05 secret-path/
//! bearer connector: `remind_me_remote::server::build_router` exactly as
//! production uses it, on an ephemeral loopback port, via `reqwest`.
//!
//! Mirrors the reference's own `tests/test_oauth.py` flow helpers
//! (`_register`, `_authorize`, `_pkce_pair`, ...) so this file's coverage
//! maps onto that one test-for-test where the scenario applies.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use remind_me_core::Database;
use remind_me_mcp::McpServer;
use serde_json::{json, Value};

const OWNER_TOKEN: &str = "owner-connector-token-ft07";
const ISSUER: &str = "http://localhost";
const REDIRECT_URI: &str = "https://claude.example/callback";

const MCP_INITIALIZE: &str = r#"{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "oauth-test", "version": "0" }
    }
}"#;

/// A `reqwest::Client` that does NOT follow redirects -- every test here
/// needs to inspect the `Location` header itself (the consent txn, the
/// issued code, the error) rather than have reqwest chase it.
fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client builder should not fail for a policy-only config")
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

/// `build_router`'s OAuth branch resolves its state-file path via
/// `remind_me_core::remote::oauth_state_file_path()` (env-var-overridable,
/// defaulting to `~/.remind-me/oauth.json`, falling back to the pre-#228
/// `~/.remind_me/oauth.json` if only that one exists) rather than taking one
/// as a parameter -- correct for the one real server process, but this
/// file's tests must never share that file (each needs its own client/token
/// population, and must never touch a developer's actual home directory).
/// `ENV_LOCK` serializes every test that spawns a server so setting
/// `REMIND_ME_REMOTE_OAUTH_STATE_FILE` per test is race-free, the same
/// convention `remind_me_core::remote`'s own test module already
/// establishes for its env-var-driven tests. A `tokio::sync::Mutex` (async-
/// aware) rather than `std::sync::Mutex`, deliberately: the guard needs to
/// stay held for a whole test body, including across `.await`s, which is
/// exactly what a std-lib guard must never do.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Acquire `ENV_LOCK` and point `REMIND_ME_REMOTE_OAUTH_STATE_FILE` at a
/// fresh temp file for `label`. Holding the returned guard for a test's
/// whole body (including across `.await`s) is intentional -- see the doc
/// above.
async fn isolated_oauth_state_file(label: &str) -> tokio::sync::MutexGuard<'static, ()> {
    let guard = ENV_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!(
        "rrm_oauth_http_test_{label}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var(
        remind_me_core::remote::REMOTE_OAUTH_STATE_FILE_ENV,
        dir.join("oauth.json"),
    );
    guard
}

/// Spin up `build_router` in OAuth mode on an ephemeral loopback port.
/// `http://localhost` is a valid issuer per `oauth::validate_issuer` (the
/// reference's own SDK carve-out for local testing), so this never needs a
/// real TLS-terminating tunnel to exercise the OAuth routes end to end.
async fn spawn_oauth_server() -> SocketAddr {
    let db = Database::open_in_memory().unwrap();
    let mcp = Arc::new(McpServer::new(db));
    let router =
        remind_me_remote::build_router(mcp, OWNER_TOKEN.to_string(), Some(ISSUER.to_string()))
            .expect("a plain http://localhost issuer must validate");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

/// RFC 7636 PKCE pair: a random verifier and its S256 challenge, using the
/// crate's own (independently unit-tested) implementation -- this file
/// tests the HTTP surface built on top of it, not PKCE math itself.
fn pkce_pair() -> (String, String) {
    let verifier = remind_me_core::remote::generate_token();
    let challenge = remind_me_remote::oauth::pkce::code_challenge_s256(&verifier);
    (verifier, challenge)
}

fn query_pairs(location: &str) -> HashMap<String, String> {
    let query = location.split_once('?').map(|(_, q)| q).unwrap_or("");
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), urlencoding_decode(v)))
        .collect()
}

/// Minimal `application/x-www-form-urlencoded` value decoder (`+` -> space,
/// `%XX` -> byte) -- just enough to read back query params this crate's own
/// `construct_redirect_uri` produced, without adding a `url`/`percent-
/// encoding` dependency for test-only code.
fn urlencoding_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) =
                    u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
                {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn register_client(client: &reqwest::Client, addr: SocketAddr, name: &str) -> Value {
    let response = client
        .post(format!("http://{addr}/register"))
        .json(&json!({
            "client_name": name,
            "redirect_uris": [REDIRECT_URI],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "client_secret_post",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201, "registration should succeed");
    response.json().await.unwrap()
}

/// Drive `/authorize` -> `/consent` (GET, to fetch the txn) -> `/consent`
/// (POST, the owner's decision) and return the final redirect `Location`.
async fn authorize_and_decide(
    client: &reqwest::Client,
    addr: SocketAddr,
    client_id: &str,
    challenge: &str,
    owner_token: &str,
    action: &str,
) -> String {
    let authorize = client
        .get(format!("http://{addr}/authorize"))
        .query(&[
            ("client_id", client_id),
            ("redirect_uri", REDIRECT_URI),
            ("response_type", "code"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", "st4te"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(authorize.status(), 302);
    let consent_location = authorize
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        consent_location.starts_with("/consent?txn="),
        "{consent_location}"
    );

    let page = client
        .get(format!("http://{addr}{consent_location}"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);

    let txn = consent_location.strip_prefix("/consent?txn=").unwrap();
    let decision = client
        .post(format!("http://{addr}/consent"))
        .form(&[
            ("txn", txn),
            ("owner_token", owner_token),
            ("action", action),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(decision.status(), 302);
    decision
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

/// Full register -> authorize -> approve -> code -> token exchange,
/// returning the issued token payload.
async fn obtain_tokens(client: &reqwest::Client, addr: SocketAddr, client_id: &str) -> Value {
    let (verifier, challenge) = pkce_pair();
    let redirect =
        authorize_and_decide(client, addr, client_id, &challenge, OWNER_TOKEN, "approve").await;
    let code = query_pairs(&redirect)
        .remove("code")
        .expect("approved redirect must carry a code");

    let response = client
        .post(format!("http://{addr}/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", client_id),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
    response.json().await.unwrap()
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[tokio::test]
async fn as_metadata_points_every_endpoint_at_the_configured_issuer() {
    let _guard = isolated_oauth_state_file("as_metadata").await;
    let addr = spawn_oauth_server().await;
    let response = reqwest::get(format!(
        "http://{addr}/.well-known/oauth-authorization-server"
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    let meta: Value = response.json().await.unwrap();
    assert_eq!(meta["issuer"], ISSUER);
    assert_eq!(
        meta["authorization_endpoint"],
        format!("{ISSUER}/authorize")
    );
    assert_eq!(meta["token_endpoint"], format!("{ISSUER}/token"));
    assert_eq!(meta["registration_endpoint"], format!("{ISSUER}/register"));
    assert_eq!(meta["revocation_endpoint"], format!("{ISSUER}/revoke"));
    assert_eq!(meta["code_challenge_methods_supported"], json!(["S256"]));
    let grant_types = meta["grant_types_supported"].as_array().unwrap();
    assert!(grant_types.contains(&json!("authorization_code")));
    assert!(grant_types.contains(&json!("refresh_token")));
}

#[tokio::test]
async fn protected_resource_metadata_is_served_at_both_the_canonical_and_alias_paths() {
    let _guard = isolated_oauth_state_file("pr_metadata").await;
    let addr = spawn_oauth_server().await;
    for path in [
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-protected-resource",
    ] {
        let response = reqwest::get(format!("http://{addr}{path}")).await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
        let meta: Value = response.json().await.unwrap();
        assert_eq!(meta["resource"], format!("{ISSUER}/mcp"));
        assert_eq!(meta["authorization_servers"], json!([ISSUER]));
    }
}

#[tokio::test]
async fn an_unauthenticated_mcp_request_401s_with_a_resource_metadata_hint() {
    let _guard = isolated_oauth_state_file("mcp_401").await;
    let addr = spawn_oauth_server().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/mcp"))
        .headers(mcp_headers())
        .body(MCP_INITIALIZE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let www_authenticate = response
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(www_authenticate.contains("resource_metadata="));
    assert!(www_authenticate.contains("/.well-known/oauth-protected-resource/mcp"));
}

// ---------------------------------------------------------------------------
// Dynamic client registration (RFC 7591)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dynamic_client_registration_issues_a_client_id_and_forces_no_secret() {
    let _guard = isolated_oauth_state_file("dcr_no_secret").await;
    let addr = spawn_oauth_server().await;
    let client = reqwest::Client::new();
    let info = register_client(&client, addr, "claude.ai").await;

    assert!(info["client_id"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(info["client_name"], "claude.ai");
    assert_eq!(info["token_endpoint_auth_method"], "none");
    // exclude-None serialization: an unissued client_secret is omitted
    // entirely, not present-with-null.
    assert!(info.get("client_secret").is_none());
    assert!(info.get("client_secret_expires_at").is_none());
}

#[tokio::test]
async fn registration_rejects_a_client_with_no_redirect_uris() {
    let _guard = isolated_oauth_state_file("dcr_no_redirects").await;
    let addr = spawn_oauth_server().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/register"))
        .json(&json!({ "redirect_uris": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"], "invalid_client_metadata");
}

// ---------------------------------------------------------------------------
// Full PKCE authorization-code flow, refresh, revocation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_pkce_flow_issue_exchange_refresh_revoke_and_an_authenticated_mcp_round_trip() {
    let _guard = isolated_oauth_state_file("full_flow").await;
    let addr = spawn_oauth_server().await;
    let client = no_redirect_client();
    let info = register_client(&client, addr, "claude.ai").await;
    let client_id = info["client_id"].as_str().unwrap();

    // Issue.
    let tokens = obtain_tokens(&client, addr, client_id).await;
    assert_eq!(tokens["token_type"], "Bearer");
    assert!(tokens["access_token"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    assert!(tokens["refresh_token"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    let access_token = tokens["access_token"].as_str().unwrap().to_string();
    let refresh_token = tokens["refresh_token"].as_str().unwrap().to_string();

    // The issued access token authenticates a real MCP session.
    let mut headers = mcp_headers();
    headers.insert(
        "Authorization",
        format!("Bearer {access_token}").parse().unwrap(),
    );
    let init = client
        .post(format!("http://{addr}/mcp"))
        .headers(headers)
        .body(MCP_INITIALIZE)
        .send()
        .await
        .unwrap();
    assert_eq!(init.status(), 200, "{:?}", init.text().await);

    // Refresh: rotates both tokens.
    let refreshed: Value = client
        .post(format!("http://{addr}/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", client_id),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rotated_access = refreshed["access_token"].as_str().unwrap();
    let rotated_refresh = refreshed["refresh_token"].as_str().unwrap();
    assert_ne!(rotated_access, access_token);
    assert_ne!(rotated_refresh, refresh_token);

    // The old refresh token is dead (rotation).
    let replay = client
        .post(format!("http://{addr}/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", client_id),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 400);
    let replay_body: Value = replay.json().await.unwrap();
    assert_eq!(replay_body["error"], "invalid_grant");

    // Revoke (RFC 7009): kills the client's whole session, not just the
    // presented token.
    let revoke = client
        .post(format!("http://{addr}/revoke"))
        .form(&[("token", rotated_refresh), ("client_id", client_id)])
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status(), 200);

    let mut dead_headers = mcp_headers();
    dead_headers.insert(
        "Authorization",
        format!("Bearer {rotated_access}").parse().unwrap(),
    );
    let after_revoke = client
        .post(format!("http://{addr}/mcp"))
        .headers(dead_headers)
        .body(MCP_INITIALIZE)
        .send()
        .await
        .unwrap();
    assert_eq!(after_revoke.status(), 401);
}

#[tokio::test]
async fn a_wrong_owner_credential_and_an_explicit_deny_produce_the_identical_denial() {
    let _guard = isolated_oauth_state_file("denial").await;
    let addr = spawn_oauth_server().await;
    let client = no_redirect_client();
    let info = register_client(&client, addr, "claude.ai").await;
    let client_id = info["client_id"].as_str().unwrap();

    let (_verifier, challenge) = pkce_pair();
    let wrong_cred = authorize_and_decide(
        &client,
        addr,
        client_id,
        &challenge,
        "not-the-owner-token",
        "approve",
    )
    .await;
    let explicit_deny =
        authorize_and_decide(&client, addr, client_id, &challenge, OWNER_TOKEN, "deny").await;

    for redirect in [&wrong_cred, &explicit_deny] {
        let params = query_pairs(redirect);
        assert_eq!(params.get("error"), Some(&"access_denied".to_string()));
        assert!(!params.contains_key("code"));
    }
}

#[tokio::test]
async fn a_pkce_mismatch_is_invalid_grant_and_the_code_is_not_consumed_by_the_attempt() {
    let _guard = isolated_oauth_state_file("pkce_mismatch").await;
    let addr = spawn_oauth_server().await;
    let client = no_redirect_client();
    let info = register_client(&client, addr, "claude.ai").await;
    let client_id = info["client_id"].as_str().unwrap();

    let (verifier, challenge) = pkce_pair();
    let redirect =
        authorize_and_decide(&client, addr, client_id, &challenge, OWNER_TOKEN, "approve").await;
    let code = query_pairs(&redirect).remove("code").unwrap();

    let (wrong_verifier, _) = pkce_pair();
    let mismatched = client
        .post(format!("http://{addr}/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", client_id),
            ("code_verifier", wrong_verifier.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(mismatched.status(), 400);
    let body: Value = mismatched.json().await.unwrap();
    assert_eq!(body["error"], "invalid_grant");

    // The correct verifier still works -- a failed PKCE check must not have
    // consumed the (still single-use) code.
    let ok = client
        .post(format!("http://{addr}/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", client_id),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200, "{:?}", ok.text().await);
}

#[tokio::test]
async fn an_authorization_code_cannot_be_exchanged_twice() {
    let _guard = isolated_oauth_state_file("single_use_code").await;
    let addr = spawn_oauth_server().await;
    let client = no_redirect_client();
    let info = register_client(&client, addr, "claude.ai").await;
    let client_id = info["client_id"].as_str().unwrap();

    let (verifier, challenge) = pkce_pair();
    let redirect =
        authorize_and_decide(&client, addr, client_id, &challenge, OWNER_TOKEN, "approve").await;
    let code = query_pairs(&redirect).remove("code").unwrap();

    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", client_id),
        ("code_verifier", verifier.as_str()),
    ];
    let first = client
        .post(format!("http://{addr}/token"))
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);

    let replay = client
        .post(format!("http://{addr}/token"))
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 400);
    let body: Value = replay.json().await.unwrap();
    assert_eq!(body["error"], "invalid_grant");
}

// ---------------------------------------------------------------------------
// remind_me_revoke_clients semantics: list (empty client_id) vs revoke-one
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoke_clients_semantics_empty_id_lists_nonempty_id_revokes_one_not_all() {
    let _guard = isolated_oauth_state_file("revoke_semantics").await;
    let addr = spawn_oauth_server().await;
    let client = no_redirect_client();
    let a = register_client(&client, addr, "client-a").await;
    let b = register_client(&client, addr, "client-b").await;
    let a_id = a["client_id"].as_str().unwrap().to_string();
    let b_id = b["client_id"].as_str().unwrap().to_string();
    let _ = obtain_tokens(&client, addr, &a_id).await;
    let _ = obtain_tokens(&client, addr, &b_id).await;

    // This is exactly what `remind_me_mcp`'s `remind_me_revoke_clients`
    // tool does under the hood (`remind_me_core::remote::OAuthStateStore`),
    // against the same state file the live server above is using.
    let state_file = remind_me_core::remote::oauth_state_file_path();
    let store = remind_me_core::remote::OAuthStateStore::new(&state_file);

    // Empty client_id: lists both clients, revokes nothing.
    let listed = store.list_clients();
    assert_eq!(
        listed.len(),
        2,
        "empty client_id must list, not revoke, every registered client"
    );
    assert!(listed.iter().any(|c| c.client_id == a_id));
    assert!(listed.iter().any(|c| c.client_id == b_id));

    // Non-empty client_id revokes exactly that one client.
    let summary = store
        .revoke_client(&a_id)
        .expect("write")
        .expect("client-a is registered");
    assert_eq!(summary.client_id, a_id);
    assert_eq!(summary.access_tokens, 1);
    assert_eq!(summary.refresh_tokens, 1);

    // client-b is untouched -- this was "revoke one", not "revoke all".
    let remaining = store.list_clients();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].client_id, b_id);

    // Revoking an unknown client_id is an error (`None`), not a silent
    // "revoked everything that happened to exist".
    assert!(store.revoke_client("no-such-client").unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Legacy secret-path/bearer coexistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_legacy_secret_path_still_completes_an_mcp_initialize_in_oauth_mode() {
    let _guard = isolated_oauth_state_file("legacy_secret_path").await;
    let addr = spawn_oauth_server().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/mcp/{OWNER_TOKEN}"))
        .headers(mcp_headers())
        .body(MCP_INITIALIZE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
}

#[tokio::test]
async fn the_legacy_bearer_token_still_authenticates_mcp_in_oauth_mode() {
    let _guard = isolated_oauth_state_file("legacy_bearer").await;
    let addr = spawn_oauth_server().await;
    let client = reqwest::Client::new();
    let mut headers = mcp_headers();
    headers.insert(
        "Authorization",
        format!("Bearer {OWNER_TOKEN}").parse().unwrap(),
    );
    let response = client
        .post(format!("http://{addr}/mcp"))
        .headers(headers)
        .body(MCP_INITIALIZE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
}

#[tokio::test]
async fn a_2026_07_28_discover_lifecycle_request_authenticates_via_oauth_bearer_with_no_session() {
    // OAuth mode (an issuer configured) and the SEP-2567 discover lifecycle
    // are independent axes -- `oauth::require_bearer` gates `/mcp` before
    // `tower.rs` ever looks at the negotiated protocol version. Nothing in
    // `http_test.rs`'s 2026-07-28 coverage runs with OAuth active, and
    // nothing here runs the discover lifecycle -- this is the one place
    // both are exercised together, using the legacy owner-token bearer
    // credential (still valid in OAuth mode, see
    // `the_legacy_bearer_token_still_authenticates_mcp_in_oauth_mode`
    // above) since a dynamically-issued OAuth access token authenticates
    // through the identical bearer check either way.
    let _guard = isolated_oauth_state_file("discover_lifecycle").await;
    let addr = spawn_oauth_server().await;
    let client = reqwest::Client::new();

    let mut headers = mcp_headers();
    headers.insert(
        "Authorization",
        format!("Bearer {OWNER_TOKEN}").parse().unwrap(),
    );
    headers.insert("MCP-Protocol-Version", "2026-07-28".parse().unwrap());
    headers.insert("Mcp-Method", "tools/list".parse().unwrap());

    let response = client
        .post(format!("http://{addr}/mcp"))
        .headers(headers)
        // The per-request `_meta` every 2026-07-28 request carries -- see
        // `discover_meta` in http_test.rs.
        .body(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200, "{:?}", response.text().await);
}

#[tokio::test]
async fn probes_still_fail_closed_in_oauth_mode() {
    let _guard = isolated_oauth_state_file("probes").await;
    let addr = spawn_oauth_server().await;
    let client = reqwest::Client::new();

    let wrong_secret_path = client
        .post(format!("http://{addr}/mcp/wrong-token"))
        .headers(mcp_headers())
        .body(MCP_INITIALIZE)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_secret_path.status(), 404);

    let mut wrong_bearer_headers = mcp_headers();
    wrong_bearer_headers.insert("Authorization", "Bearer nope".parse().unwrap());
    let wrong_bearer = client
        .post(format!("http://{addr}/mcp"))
        .headers(wrong_bearer_headers)
        .body(MCP_INITIALIZE)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_bearer.status(), 401);

    let unrelated = client
        .get(format!("http://{addr}/api/stats"))
        .send()
        .await
        .unwrap();
    assert_eq!(unrelated.status(), 404);

    let health = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
}

// ---------------------------------------------------------------------------
// Issuer validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn build_router_rejects_an_issuer_with_a_path_or_plain_http_on_a_non_local_host() {
    let db = Database::open_in_memory().unwrap();
    let mcp = Arc::new(McpServer::new(db));
    assert!(remind_me_remote::build_router(
        Arc::clone(&mcp),
        OWNER_TOKEN.to_string(),
        Some("https://machine.example/path".to_string()),
    )
    .is_err());

    let db2 = Database::open_in_memory().unwrap();
    let mcp2 = Arc::new(McpServer::new(db2));
    assert!(remind_me_remote::build_router(
        mcp2,
        OWNER_TOKEN.to_string(),
        Some("http://machine.example".to_string()),
    )
    .is_err());
}

#[tokio::test]
async fn build_router_without_an_issuer_serves_the_plain_ft05_app_with_no_oauth_routes() {
    let db = Database::open_in_memory().unwrap();
    let mcp = Arc::new(McpServer::new(db));
    let router = remind_me_remote::build_router(mcp, OWNER_TOKEN.to_string(), None).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    for path in [
        "/authorize",
        "/token",
        "/register",
        "/.well-known/oauth-authorization-server",
    ] {
        let response = reqwest::get(format!("http://{addr}{path}")).await.unwrap();
        assert_eq!(
            response.status(),
            404,
            "{path} must not exist without an issuer"
        );
    }
    let health = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(health.status(), 200);
}
