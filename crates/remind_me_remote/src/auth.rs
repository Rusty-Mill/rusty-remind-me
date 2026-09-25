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
//! Implemented as an axum middleware ([`secret_gate`], via
//! `axum::middleware::from_fn_with_state`) applied at the `Router` level, so
//! it runs — and can rewrite [`http::Uri`] — before the inner router matches
//! a route, the same ordering the reference's ASGI middleware relies on.
//!
//! The token comparison reuses [`remind_me_core::webhook::constant_time_eq`]
//! rather than a new implementation next to it, per this crate's own
//! precedent (`remind_me_api`'s `http.rs`).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::extract::{Request, State};
use axum::http::uri::PathAndQuery;
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use remind_me_core::webhook::constant_time_eq;
use serde_json::json;
use std::net::SocketAddr;

/// The one path the MCP transport is mounted at. Not configurable: nothing
/// in this crate exposes a different mount point, unlike the reference
/// (which derives it from `mcp.settings.streamable_http_path`).
pub const MCP_PATH: &str = "/mcp";

/// The health probe path, always unauthenticated.
pub const HEALTH_PATH: &str = "/health";

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "Not found" }))).into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "Unauthorized" })),
    )
        .into_response()
}

/// Rewrite `uri`'s path to `new_path`, preserving any query string.
///
/// `None` only if `new_path` (built entirely from this module's own
/// constants and validated path segments) somehow fails to parse as a valid
/// `PathAndQuery` -- callers treat that as a 404 rather than panicking, but
/// it is not expected to occur in practice.
fn rewrite_path(uri: &Uri, new_path: &str) -> Option<Uri> {
    let mut parts = uri.clone().into_parts();
    let path_and_query = match uri.query() {
        Some(query) => format!("{new_path}?{query}"),
        None => new_path.to_string(),
    };
    parts.path_and_query = Some(PathAndQuery::try_from(path_and_query).ok()?);
    Uri::from_parts(parts).ok()
}

/// [`secret_gate`]'s configuration — a direct port of the reference's
/// `SecretPathMiddleware.__init__` parameters.
pub struct GateConfig {
    /// The resolved connector token (never logged, never included in a
    /// response). In OAuth mode it doubles as the owner credential the
    /// `/consent` page checks, but that comparison lives in
    /// `oauth::Provider`, not here.
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
}

impl GateConfig {
    /// Legacy (FT-05, no OAuth) configuration: only `/health` is exempt.
    pub fn legacy(token: String) -> Self {
        Self {
            token,
            oauth_mode: false,
            extra_allow_paths: &[],
            allow_prefixes: &[],
        }
    }
}

/// Whatever credential this request would be authenticated by: the secret
/// path segment in secret-path mode, otherwise the bearer token.
fn presented_credential(request: &Request<Body>, path: &str) -> String {
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
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string()
}

/// 429 with `Retry-After`, which is what tells a rejected client when to
/// come back rather than immediately retrying into the same wall.
fn too_many_requests(retry_after: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, retry_after.to_string())],
        Json(json!({ "error": "rate limit exceeded" })),
    )
        .into_response()
}

/// The axum middleware itself.
pub async fn secret_gate(
    State(config): State<Arc<GateConfig>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();

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
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_default();
    let bucket =
        remind_me_core::rate_limit::resolve_key(&presented, &peer, Some(config.token.as_str()));
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
        return match rewrite_path(request.uri(), &new_path) {
            Some(rewritten) => {
                *request.uri_mut() = rewritten;
                if config.oauth_mode {
                    // Re-express the secret path as a bearer credential so
                    // `oauth::require_bearer` authenticates it like any
                    // other token.
                    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", config.token)) {
                        request.headers_mut().remove(header::AUTHORIZATION);
                        request.headers_mut().insert(header::AUTHORIZATION, value);
                    }
                }
                next.run(request).await
            }
            None => not_found(),
        };
    }

    if path == MCP_PATH {
        if config.oauth_mode {
            // `oauth::require_bearer`, layered only onto this route, owns
            // /mcp auth in OAuth mode (accepting OAuth access tokens AND
            // the legacy connector token via `Provider::load_access_token`).
            return next.run(request).await;
        }
        let authorization = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
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

    #[test]
    fn rewrite_path_preserves_an_existing_query_string() {
        let uri: Uri = "/mcp/sekrit?foo=bar".parse().unwrap();
        let rewritten = rewrite_path(&uri, MCP_PATH).unwrap();
        assert_eq!(rewritten, "/mcp?foo=bar");
    }

    #[test]
    fn rewrite_path_with_no_query_string() {
        let uri: Uri = "/mcp/sekrit".parse().unwrap();
        let rewritten = rewrite_path(&uri, MCP_PATH).unwrap();
        assert_eq!(rewritten, "/mcp");
    }
}
