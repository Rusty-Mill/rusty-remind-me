//! Secret-path / bearer gate for the remote MCP connector (FT-05/FT-07).
//!
//! Ported from the reference's `SecretPathMiddleware`
//! (`remind_me_mcp/remote.py`), both branches. Admits a request when either:
//!
//! - its path is `/mcp/<token>` (optionally with a trailing segment) — the
//!   path is rewritten to `/mcp` (or `/mcp/<rest>`) and forwarded (in OAuth
//!   mode the matched token is also injected as an `Authorization: Bearer`
//!   header, so `oauth::require_bearer` — layered separately, only onto the
//!   `/mcp` route — authenticates it exactly like any other bearer token);
//!   or
//! - its path is exactly `/mcp`. In legacy mode it must carry
//!   `Authorization: Bearer <token>`; in OAuth mode it is forwarded as-is —
//!   `oauth::require_bearer` decides (401 with a `WWW-Authenticate` hint
//!   pointing at the resource metadata, which is how clients discover the
//!   authorization server).
//!
//! A third credential, the optional capture-only token (ADR-0036), is
//! accepted only as `Authorization: Bearer` on `POST /mcp`, and only for a
//! single JSON-RPC message that opens a session or calls
//! `remind_me_capture_transcript` in one ([`capture_body`]). Anything else
//! it is sent with is answered 403. An admitted request is passed on with
//! its body re-serialised and the connector token, so the routes behind the
//! gate need no third case.
//!
//! `/health` always passes through unauthenticated (SE-04 parity).
//! [`GateConfig::extra_allow_paths`]/[`GateConfig::allow_prefixes`] (OAuth's
//! `/authorize`, `/token`, `/register`, `/revoke`, `/consent`, and the
//! `/.well-known/` metadata documents) pass through untouched too — those
//! routes authenticate themselves (owner-credential consent, client_id
//! lookup, or nothing at all for public metadata). Every other path is
//! answered 404 without distinguishing "wrong token" from "not a real path"
//! — a path-based probe learns nothing. A syntactically plausible
//! `/mcp/<token>` with the wrong token is also 404, matching the reference
//! exactly (only the header-based `/mcp` form uses 401 in legacy mode, since
//! that form has no ambiguity about which endpoint it targets).
//!
//! Implemented as a `rusty_axum` middleware ([`secret_gate`], via
//! `rusty_axum::middleware::from_fn`) applied at the `Router` level, so it
//! runs — and can rewrite the request's path — before the inner router
//! matches a route, the same ordering the reference's ASGI middleware relies
//! on.
//!
//! The token comparison reuses [`remind_me_core::webhook::constant_time_eq`]
//! rather than a new implementation next to it, per this crate's own
//! precedent (`remind_me_api`'s `http.rs`).

use std::sync::Arc;

use remind_me_core::webhook::constant_time_eq;
use rusty_axum::middleware::Next;
use rusty_axum::{IntoResponse, Json, Method, Request, Response, StatusCode};
use serde_json::{json, Value};

/// The one path the MCP transport is mounted at. Not configurable: nothing
/// in this crate exposes a different mount point, unlike the reference
/// (which derives it from `mcp.settings.streamable_http_path`).
pub const MCP_PATH: &str = "/mcp";

/// The health probe path, always unauthenticated.
pub const HEALTH_PATH: &str = "/health";

pub use remind_me_mcp::{CAPTURE_CLIENT, CAPTURE_TOOL};

/// JSON-RPC methods the capture-only credential may send besides
/// `tools/call`: what a client needs to open a session and keep it alive.
const CAPTURE_SESSION_METHODS: [&str; 3] = ["initialize", "notifications/initialized", "ping"];

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "Not found" }))).into_response()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": "Forbidden" }))).into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "Unauthorized" })),
    )
        .into_response()
}

/// `request` addressed to `new_path` instead, preserving any query string,
/// its headers, body and peer address.
fn rewrite_path(mut request: Request, new_path: &str) -> Request {
    let target = match request.query() {
        Some(query) => format!("{new_path}?{query}"),
        None => new_path.to_string(),
    };
    let body = request.take_body();
    let rewritten = Request::new(request.method().clone(), target)
        .with_headers(request.headers().clone())
        .with_body(body);
    match request.peer_addr() {
        Some(peer) => rewritten.with_peer_addr(peer),
        None => rewritten,
    }
}

/// [`secret_gate`]'s configuration — a direct port of the reference's
/// `SecretPathMiddleware.__init__` parameters.
pub struct GateConfig {
    /// The resolved connector token (never logged, never included in a
    /// response). In OAuth mode it doubles as the owner credential the
    /// `/consent` page checks, but that comparison lives in
    /// `oauth::OAuthAppState::new`, not here.
    pub token: String,
    /// `true` once `REMIND_ME_REMOTE_ISSUER` is set: `/mcp` (bare, no
    /// matching secret-path segment) is forwarded rather than gated on the
    /// legacy bearer check, and the secret-path rewrite additionally
    /// injects `Authorization: Bearer <token>`.
    pub oauth_mode: bool,
    /// Exact paths that pass through unauthenticated besides `/health` —
    /// OAuth's `/authorize`, `/token`, `/register`, `/revoke`, `/consent`.
    /// Empty in legacy mode.
    pub extra_allow_paths: &'static [&'static str],
    /// Path prefixes that pass through unauthenticated — OAuth's
    /// `/.well-known/` metadata documents. Empty in legacy mode.
    pub allow_prefixes: &'static [&'static str],
    /// The capture-only credential (ADR-0036), when one is configured and
    /// usable. Never logged, never included in a response.
    pub capture_token: Option<String>,
}

impl GateConfig {
    /// Legacy (FT-05, no OAuth) configuration: only `/health` is exempt.
    pub fn legacy(token: String, capture_token: Option<String>) -> Self {
        Self {
            token,
            oauth_mode: false,
            extra_allow_paths: &[],
            allow_prefixes: &[],
            capture_token,
        }
    }

    /// Whether `presented` is the capture-only credential.
    fn is_capture(&self, presented: &str) -> bool {
        self.capture_token.as_deref().is_some_and(|capture| {
            !presented.is_empty() && constant_time_eq(presented.as_bytes(), capture.as_bytes())
        })
    }
}

/// The body the capture-only credential may send, as it will be forwarded,
/// or `None` to refuse. Admitted: a `POST /mcp` carrying one JSON-RPC 2.0
/// message (no batch) that opens or keeps a session, or calls
/// [`CAPTURE_TOOL`] within one (`Mcp-Session-Id`), so the caller's identity
/// always comes from a handshake this gate saw.
///
/// The parsed message is re-serialised, so what was checked is exactly what
/// the server reads (no duplicate-key differential). On `initialize` the
/// client's name is pinned to [`CAPTURE_CLIENT`], so every write in that
/// session is marked as made with this credential. `Mcp-Method` and
/// `Mcp-Name` headers, when present, must agree with the body.
pub fn capture_body(request: &Request) -> Option<Vec<u8>> {
    if *request.method() != Method::Post || request.path() != MCP_PATH {
        return None;
    }
    let mut message = serde_json::from_slice::<Value>(request.body()).ok()?;
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return None;
    }
    let method = message.get("method").and_then(Value::as_str)?.to_string();
    let headers = request.headers();
    if headers.get("mcp-method").is_some_and(|h| h != method) {
        return None;
    }
    let mcp_name = headers.get("mcp-name");
    if CAPTURE_SESSION_METHODS.contains(&method.as_str()) {
        if mcp_name.is_some() {
            return None;
        }
        if method == "initialize" {
            pin_client(&mut message)?;
        }
        return serde_json::to_vec(&message).ok();
    }
    let tool = message.pointer("/params/name").and_then(Value::as_str);
    let allowed = method == "tools/call"
        && tool == Some(CAPTURE_TOOL)
        && mcp_name.is_none_or(|h| h == CAPTURE_TOOL)
        && headers.get("mcp-session-id").is_some();
    allowed.then(|| serde_json::to_vec(&message).ok()).flatten()
}

/// Set an `initialize` message's `params.clientInfo.name` to
/// [`CAPTURE_CLIENT`], keeping its version.
fn pin_client(message: &mut Value) -> Option<()> {
    let params = message.get_mut("params")?.as_object_mut()?;
    let info = params
        .entry("clientInfo")
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    info.insert("name".to_string(), json!(CAPTURE_CLIENT));
    info.entry("version").or_insert_with(|| json!("0"));
    Some(())
}

/// A request presenting the capture-only credential: refused unless
/// [`capture_body`] admits it, then passed on with that body and the
/// connector token, as a connector-token request would be.
async fn admit_capture(config: &GateConfig, request: Request, next: Next) -> Response {
    let Some(body) = capture_body(&request) else {
        return forbidden();
    };
    let mut request = request.with_body(body);
    let bearer = format!("Bearer {}", config.token);
    if request
        .headers_mut()
        .insert_sensitive("authorization", &bearer)
        .is_err()
    {
        return forbidden();
    }
    next.run(request).await
}

/// Whatever credential this request would be authenticated by: the secret
/// path segment in secret-path mode, otherwise the bearer token.
fn presented_credential(request: &Request, path: &str) -> String {
    let prefix = format!("{MCP_PATH}/");
    if let Some(after_prefix) = path.strip_prefix(&prefix) {
        return after_prefix
            .split_once('/')
            .map(|(segment, _)| segment)
            .unwrap_or(after_prefix)
            .to_string();
    }
    request
        .headers()
        .get("authorization")
        .and_then(rusty_oauth::bearer::token_from_authorization)
        .unwrap_or("")
        .to_string()
}

/// 429 with `Retry-After`, which is what tells a rejected client when to
/// come back rather than immediately retrying into the same wall.
fn too_many_requests(retry_after: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "error": "rate limit exceeded" })),
    )
        .into_response()
        .with_header("retry-after", &retry_after.to_string())
}

/// The middleware itself.
pub async fn secret_gate(config: Arc<GateConfig>, request: Request, next: Next) -> Response {
    let path = request.path().to_string();

    if path == HEALTH_PATH
        || config.extra_allow_paths.contains(&path.as_str())
        || config
            .allow_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
    {
        return next.run(request).await;
    }

    // Rate limited before any credential is checked (issue #121). This
    // endpoint is reachable from the internet when tunnelled, and a limiter
    // that only engaged after a valid token would leave an unauthenticated
    // flood entirely unbounded — the flood that actually matters.
    //
    // The credential is taken from whichever place this request would carry
    // it, so a caller presenting the right one lands in the shared
    // `auth:known` bucket either way rather than being limited per address.
    let presented = presented_credential(&request, &path);
    let peer = request
        .peer_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_default();
    let capture = config.is_capture(&presented);
    let bucket = if capture {
        "auth:capture".to_string()
    } else {
        remind_me_core::rate_limit::resolve_key(&presented, &peer, Some(config.token.as_str()))
    };
    // Synchronous, and safe to call from this async context: the critical
    // section is a map update with no I/O and nothing awaited while the lock
    // is held, so it cannot block the executor for longer than that.
    if let Some(verdict) = remind_me_core::rate_limit::check(&bucket) {
        if !verdict.allowed {
            return too_many_requests(remind_me_core::rate_limit::retry_after_seconds(
                verdict.retry_after,
            ));
        }
    }

    if capture {
        return admit_capture(&config, request, next).await;
    }

    let prefix = format!("{MCP_PATH}/");
    if let Some(after_prefix) = path.strip_prefix(&prefix) {
        let (segment, rest) = match after_prefix.split_once('/') {
            Some((segment, rest)) => (segment, Some(rest)),
            None => (after_prefix, None),
        };
        if segment.is_empty() || !constant_time_eq(segment.as_bytes(), config.token.as_bytes()) {
            return not_found();
        }
        let new_path = match rest {
            Some(rest) if !rest.is_empty() => format!("{MCP_PATH}/{rest}"),
            _ => MCP_PATH.to_string(),
        };
        let mut request = rewrite_path(request, &new_path);
        if config.oauth_mode {
            // Re-express the secret path as a bearer credential so
            // `oauth::require_bearer` authenticates it like any other token.
            // A token that is not a valid header value goes without one, and
            // `require_bearer` answers 401.
            let bearer = format!("Bearer {}", config.token);
            if request
                .headers_mut()
                .insert_sensitive("authorization", &bearer)
                .is_err()
            {
                request.headers_mut().remove("authorization");
            }
        }
        return next.run(request).await;
    }

    if path == MCP_PATH {
        if config.oauth_mode {
            // `oauth::require_bearer`, layered only onto this route, owns
            // /mcp auth in OAuth mode (accepting OAuth access tokens AND
            // the legacy connector token via `Provider::load_access_token`).
            return next.run(request).await;
        }
        let authorization = request.headers().get("authorization").unwrap_or("");
        let expected = format!("Bearer {}", config.token);
        return if constant_time_eq(authorization.as_bytes(), expected.as_bytes()) {
            next.run(request).await
        } else {
            unauthorized()
        };
    }

    not_found()
}

#[cfg(test)]
mod tests {
    use super::*;

    use rusty_axum::Method;

    fn capture_request(body: Value, session: bool) -> Request {
        let request =
            Request::new(Method::Post, MCP_PATH).with_body(serde_json::to_vec(&body).unwrap());
        if session {
            request.with_header("mcp-session-id", "s1").unwrap()
        } else {
            request
        }
    }

    fn message(method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
    }

    fn forwarded(request: &Request) -> Option<Value> {
        capture_body(request).map(|body| serde_json::from_slice(&body).unwrap())
    }

    #[test]
    fn capture_admits_a_session_and_the_capture_tool_within_it() {
        for method in CAPTURE_SESSION_METHODS {
            assert!(
                forwarded(&capture_request(message(method, json!({})), false)).is_some(),
                "{method}"
            );
        }
        let call = message(
            "tools/call",
            json!({ "name": CAPTURE_TOOL, "arguments": {} }),
        );
        assert!(forwarded(&capture_request(call.clone(), true)).is_some());
        let headed = capture_request(call, true)
            .with_header("mcp-method", "tools/call")
            .unwrap()
            .with_header("mcp-name", CAPTURE_TOOL)
            .unwrap();
        assert!(forwarded(&headed).is_some());
    }

    #[test]
    fn capture_pins_the_client_name_on_initialize() {
        let init = message(
            "initialize",
            json!({ "clientInfo": { "name": "claude-ai", "version": "2" } }),
        );
        let sent = forwarded(&capture_request(init, false)).unwrap();
        assert_eq!(
            sent["params"]["clientInfo"],
            json!({ "name": CAPTURE_CLIENT, "version": "2" })
        );
        let bare = forwarded(&capture_request(message("initialize", json!({})), false)).unwrap();
        assert_eq!(bare["params"]["clientInfo"]["name"], json!(CAPTURE_CLIENT));
    }

    #[test]
    fn capture_refuses_every_other_call_and_shape() {
        let refused = [
            message(
                "tools/call",
                json!({ "name": "remind_me_search", "arguments": {} }),
            ),
            message(
                "tools/call",
                json!({ "name": "remind_me_delete", "arguments": {} }),
            ),
            message("tools/call", json!({})),
            message("tools/list", json!({})),
            message("resources/read", json!({ "uri": "remind-me://stats" })),
            json!({ "id": 1, "method": "ping" }),
            json!([message("ping", json!({}))]),
        ];
        for body in refused {
            assert!(
                capture_body(&capture_request(body.clone(), true)).is_none(),
                "{body}"
            );
        }
        let raw = Request::new(Method::Post, MCP_PATH).with_body(b"not json".to_vec());
        assert!(capture_body(&raw).is_none());
    }

    #[test]
    fn capture_refuses_a_call_outside_a_session_and_other_routes() {
        let call = message("tools/call", json!({ "name": CAPTURE_TOOL }));
        assert!(capture_body(&capture_request(call.clone(), false)).is_none());
        let get = Request::new(Method::Get, MCP_PATH)
            .with_header("mcp-session-id", "s1")
            .unwrap();
        assert!(capture_body(&get).is_none());
        let deeper = Request::new(Method::Post, "/mcp/x")
            .with_body(serde_json::to_vec(&message("ping", json!({}))).unwrap());
        assert!(capture_body(&deeper).is_none());
    }

    #[test]
    fn capture_forwards_what_it_checked_not_duplicate_keys() {
        let raw = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"remind_me_delete","name":"remind_me_capture_transcript"}}"#;
        let request = Request::new(Method::Post, MCP_PATH)
            .with_body(raw.to_vec())
            .with_header("mcp-session-id", "s1")
            .unwrap();
        if let Some(body) = capture_body(&request) {
            let text = String::from_utf8(body).unwrap();
            assert!(!text.contains("remind_me_delete"), "{text}");
        }
    }

    #[test]
    fn capture_refuses_headers_that_disagree_with_the_body() {
        let call = message("tools/call", json!({ "name": CAPTURE_TOOL }));
        let wrong_method = capture_request(call.clone(), true)
            .with_header("mcp-method", "tools/list")
            .unwrap();
        assert!(capture_body(&wrong_method).is_none());
        let wrong_name = capture_request(call, true)
            .with_header("mcp-name", "remind_me_delete")
            .unwrap();
        assert!(capture_body(&wrong_name).is_none());
        let ping = capture_request(message("ping", json!({})), false)
            .with_header("mcp-name", CAPTURE_TOOL)
            .unwrap();
        assert!(capture_body(&ping).is_none());
    }

    #[test]
    fn rewrite_path_preserves_an_existing_query_string() {
        let request = Request::new(Method::Get, "/mcp/sekrit?foo=bar");
        assert_eq!(rewrite_path(request, MCP_PATH).target(), "/mcp?foo=bar");
    }

    #[test]
    fn rewrite_path_with_no_query_string() {
        let request = Request::new(Method::Get, "/mcp/sekrit");
        assert_eq!(rewrite_path(request, MCP_PATH).target(), "/mcp");
    }

    #[test]
    fn rewrite_path_keeps_method_headers_body_and_peer() {
        let peer = std::net::SocketAddr::from(([10, 0, 0, 7], 4000));
        let request = Request::new(Method::Post, "/mcp/sekrit")
            .with_header("x-trace", "t1")
            .unwrap()
            .with_body(b"{}".to_vec())
            .with_peer_addr(peer);
        let rewritten = rewrite_path(request, MCP_PATH);
        assert_eq!(*rewritten.method(), Method::Post);
        assert_eq!(rewritten.headers().get("x-trace"), Some("t1"));
        assert_eq!(rewritten.body(), b"{}");
        assert_eq!(rewritten.peer_addr(), Some(peer));
    }
}
