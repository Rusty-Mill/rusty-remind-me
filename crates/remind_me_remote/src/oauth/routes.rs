//! HTTP surface of the OAuth authorization server (FT-07, `#86`): RFC 8414
//! AS metadata, RFC 9728 protected-resource metadata, `/authorize` +
//! `/consent` (GET+POST), `/token`, RFC 7591 `/register`, RFC 7009
//! `/revoke`, and the bearer-auth gate in front of `/mcp`.
//!
//! Every handler that touches [`Provider`] (and therefore
//! [`remind_me_core::remote::OAuthStateStore`]'s small JSON file) runs that
//! call through [`run_blocking`] — `tokio::task::spawn_blocking`, mirroring
//! the reference's own `asyncio.to_thread` (PF-06 conventions) and this
//! crate's established precedent in `handler.rs`.
//!
//! # A deliberate simplification: no client-secret verification
//!
//! The reference's SDK routes go through a general-purpose
//! `ClientAuthenticator` that can verify `client_secret_basic`/`_post`.
//! [`Provider::register_client`] always forces every client's
//! `token_endpoint_auth_method` to `"none"` and `client_secret` to `None` —
//! so for *this* provider, that general machinery's secret-comparison
//! branch is provably dead code: `client.client_secret` is always `None`,
//! and the reference's own authenticator only compares a secret when
//! `client.client_secret` is truthy. `/token` and `/revoke` below reduce
//! client "authentication" to what actually happens for a client this
//! server can ever issue: `client_id` must name a registered client. This
//! is not a security gap relative to the reference — it is the exact same
//! outcome that reference's code already always produces here — but it is
//! also not a generic multi-provider `ClientAuthenticator`, so it's called
//! out explicitly per this crate's ADR.
//!
//! # A minor, documented deviation in `/authorize` error routing
//!
//! The reference resolves the client only *after* validating the request
//! shape (`response_type`, `code_challenge`, ...), and on a validation
//! failure makes a "last-ditch" attempt to load the client anyway so it can
//! decide whether to answer with a redirect or a bare JSON error. This
//! module resolves the client *first*, then validates everything else,
//! answering with a redirect once the client and its `redirect_uri` are
//! both known. The two orders produce identical responses for every case
//! the reference's own test suite exercises (unknown client → JSON;
//! malformed `redirect_uri` → JSON; every other error → redirect) and
//! differ only in the narrow corner of a malformed field arriving
//! alongside an unknown `client_id`, which is JSON either way.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Form, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::issuer::Issuer;
use super::provider::{
    now_unix, AuthorizationParams, ConsentOutcome, Provider, RegisterError, CONSENT_PATH,
};
use super::types::ClientMetadata;

const AUTHORIZATION_PATH: &str = "/authorize";
const TOKEN_PATH: &str = "/token";
const REGISTRATION_PATH: &str = "/register";
const REVOCATION_PATH: &str = "/revoke";
const AS_METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
const AS_METADATA_ALIAS_PATH: &str = "/.well-known/oauth-authorization-server/mcp";
const PR_METADATA_PATH: &str = "/.well-known/oauth-protected-resource/mcp";
const PR_METADATA_ALIAS_PATH: &str = "/.well-known/oauth-protected-resource";

/// Shared state for every OAuth route and the `/mcp` bearer gate.
#[derive(Clone)]
pub struct OAuthAppState {
    pub provider: Arc<Provider>,
    pub issuer: Issuer,
}

/// The RFC 9728 protected-resource metadata URL for `/mcp` under `issuer` —
/// what the `WWW-Authenticate` challenge on a failed bearer check points at,
/// and what `[create_protected_resource_routes]`'s canonical route serves.
pub fn resource_metadata_url(issuer: &Issuer) -> String {
    format!("{}{PR_METADATA_PATH}", issuer.as_str())
}

/// A [`run_blocking`] closure panicked. Rendered as a generic 500.
///
/// Its own zero-sized type rather than a ready-made `Response` in the `Err`
/// arm: a `Response` is 128+ bytes, which every `Ok` path would carry too
/// (`clippy::result_large_err`).
#[derive(Debug, Clone, Copy)]
struct BlockingTaskPanicked;

impl IntoResponse for BlockingTaskPanicked {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "server_error" })),
        )
            .into_response()
    }
}

/// Run a blocking (file-I/O-touching) closure off the async runtime,
/// mapping a task panic to a generic 500 rather than propagating it —
/// no `unwrap`/`expect` on the join result in non-test code.
async fn run_blocking<T, F>(f: F) -> Result<T, BlockingTaskPanicked>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| BlockingTaskPanicked)
}

// ---------------------------------------------------------------------------
// Metadata (RFC 8414 / RFC 9728)
// ---------------------------------------------------------------------------

async fn as_metadata(State(state): State<OAuthAppState>) -> Response {
    let body = json!({
        "issuer": state.issuer.as_str(),
        "authorization_endpoint": state.issuer.endpoint(AUTHORIZATION_PATH),
        "token_endpoint": state.issuer.endpoint(TOKEN_PATH),
        "registration_endpoint": state.issuer.endpoint(REGISTRATION_PATH),
        "revocation_endpoint": state.issuer.endpoint(REVOCATION_PATH),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
        "revocation_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
        "code_challenge_methods_supported": ["S256"],
    });
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "public, max-age=3600")],
        Json(body),
    )
        .into_response()
}

async fn protected_resource_metadata(State(state): State<OAuthAppState>) -> Response {
    let resource = format!("{}{}", state.issuer.as_str(), crate::auth::MCP_PATH);
    let body = json!({
        "resource": resource,
        "authorization_servers": [state.issuer.as_str()],
        "bearer_methods_supported": ["header"],
        "resource_name": "remind_me",
    });
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "public, max-age=3600")],
        Json(body),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Redirect URI construction (mirrors the reference's `construct_redirect_uri`)
// ---------------------------------------------------------------------------

/// Percent-encode one query component, application/x-www-form-urlencoded
/// style (space → `+`) — matching `urllib.parse.urlencode`'s own encoding,
/// which is what the reference's `construct_redirect_uri` uses.
fn urlencode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Append `params` to `base`'s query string, preserving whatever query it
/// already has. `None` values are dropped (matches the reference: "if v is
/// not None").
fn construct_redirect_uri(base: &str, params: &[(&str, Option<&str>)]) -> String {
    let (path, existing_query) = match base.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (base, None),
    };
    let mut pieces: Vec<String> = existing_query
        .map(|q| {
            q.split('&')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for (key, value) in params {
        if let Some(v) = value {
            pieces.push(format!(
                "{}={}",
                urlencode_component(key),
                urlencode_component(v)
            ));
        }
    }
    if pieces.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", pieces.join("&"))
    }
}

fn redirect_to(location: &str) -> Response {
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, location),
            (header::CACHE_CONTROL, "no-store"),
        ],
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// /authorize (GET + POST)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct AuthorizeParams {
    client_id: Option<String>,
    redirect_uri: Option<String>,
    response_type: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    resource: Option<String>,
}

async fn authorize_get(
    State(state): State<OAuthAppState>,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    handle_authorize(state, params).await
}

async fn authorize_post(
    State(state): State<OAuthAppState>,
    Form(params): Form<AuthorizeParams>,
) -> Response {
    handle_authorize(state, params).await
}

fn authorize_json_error(error: &str, description: &str, request_state: Option<String>) -> Response {
    let mut body = json!({ "error": error, "error_description": description });
    if let Some(s) = request_state {
        body["state"] = json!(s);
    }
    (
        StatusCode::BAD_REQUEST,
        [(header::CACHE_CONTROL, "no-store")],
        Json(body),
    )
        .into_response()
}

fn authorize_redirect_error(
    redirect_uri: &str,
    error: &str,
    description: &str,
    state: Option<&str>,
) -> Response {
    let location = construct_redirect_uri(
        redirect_uri,
        &[
            ("error", Some(error)),
            ("error_description", Some(description)),
            ("state", state),
        ],
    );
    redirect_to(&location)
}

async fn handle_authorize(state: OAuthAppState, params: AuthorizeParams) -> Response {
    let now = now_unix();
    let request_state = params.state.clone();

    let Some(client_id) = params.client_id.clone() else {
        return authorize_json_error("invalid_request", "client_id is required", request_state);
    };

    let provider = Arc::clone(&state.provider);
    let lookup_id = client_id.clone();
    let client = match run_blocking(move || provider.get_client(&lookup_id)).await {
        Ok(v) => v,
        Err(panicked) => return panicked.into_response(),
    };
    let Some(client) = client else {
        return authorize_json_error(
            "invalid_request",
            &format!("Client ID '{client_id}' not found"),
            request_state,
        );
    };

    let redirect_uri = match client.validate_redirect_uri(params.redirect_uri.as_deref()) {
        Ok(uri) => uri,
        Err(_) => {
            return authorize_json_error(
                "invalid_request",
                "redirect_uri not registered for client",
                request_state,
            );
        }
    };

    // Client + redirect_uri are both resolved from here on, so every
    // remaining failure redirects with an error (RFC 6749 §4.1.2.1) instead
    // of answering with a bare JSON body.
    match params.response_type.as_deref() {
        Some("code") => {}
        Some(_) => {
            return authorize_redirect_error(
                &redirect_uri,
                "unsupported_response_type",
                "response_type must be 'code'",
                request_state.as_deref(),
            )
        }
        None => {
            return authorize_redirect_error(
                &redirect_uri,
                "invalid_request",
                "response_type is required",
                request_state.as_deref(),
            )
        }
    }

    let Some(code_challenge) = params.code_challenge.clone() else {
        return authorize_redirect_error(
            &redirect_uri,
            "invalid_request",
            "code_challenge is required",
            request_state.as_deref(),
        );
    };
    if let Some(method) = params.code_challenge_method.as_deref() {
        if method != "S256" {
            return authorize_redirect_error(
                &redirect_uri,
                "invalid_request",
                "code_challenge_method must be S256",
                request_state.as_deref(),
            );
        }
    }

    let scopes = match client.validate_scope(params.scope.as_deref()) {
        Ok(scopes) => scopes,
        Err(description) => {
            return authorize_redirect_error(
                &redirect_uri,
                "invalid_scope",
                &description,
                request_state.as_deref(),
            )
        }
    };

    let auth_params = AuthorizationParams {
        state: request_state,
        scopes,
        code_challenge,
        redirect_uri: redirect_uri.clone(),
        redirect_uri_provided_explicitly: params.redirect_uri.is_some(),
        resource: params.resource.clone(),
    };

    let provider = Arc::clone(&state.provider);
    let cid = client_id.clone();
    let location = match run_blocking(move || provider.authorize(&cid, auth_params, now)).await {
        Ok(v) => v,
        Err(panicked) => return panicked.into_response(),
    };
    redirect_to(&location)
}

// ---------------------------------------------------------------------------
// /consent (GET + POST)
// ---------------------------------------------------------------------------

const EXPIRED_HTML: &str = "<!doctype html><html><body><h2>Authorization request expired</h2>\
<p>This consent link is no longer valid. Retry the connection from your client.</p></body></html>";

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

fn consent_html(client_name: &str, redirect_uri: &str, txn: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>remind_me — authorize connector</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 26rem; margin: 4rem auto; padding: 0 1rem; }}
  .card {{ border: 1px solid #ddd; border-radius: 8px; padding: 1.5rem; }}
  input[type=password] {{ width: 100%; padding: .5rem; margin: .75rem 0; box-sizing: border-box; }}
  button {{ padding: .5rem 1.25rem; border-radius: 6px; border: 1px solid #888; cursor: pointer; }}
  button.approve {{ background: #1a7f37; color: #fff; border-color: #1a7f37; }}
  code {{ background: #f4f4f4; padding: 0 .25rem; }}
</style></head><body>
<div class="card">
  <h2>Authorize connector</h2>
  <p><strong>{client}</strong> wants to access your remind_me memory store.</p>
  <p>Redirects to: <code>{redirect}</code></p>
  <form method="post" action="consent">
    <input type="hidden" name="txn" value="{txn}">
    <label for="owner_token">Owner token (the remote connector token)</label>
    <input type="password" id="owner_token" name="owner_token" autocomplete="off" autofocus>
    <button class="approve" name="action" value="approve">Approve</button>
    <button name="action" value="deny">Deny</button>
  </form>
</div>
</body></html>
"#,
        client = html_escape(client_name),
        redirect = html_escape(redirect_uri),
        txn = html_escape(txn),
    )
}

#[derive(Debug, Deserialize, Default)]
struct ConsentQuery {
    txn: Option<String>,
}

async fn consent_get(
    State(state): State<OAuthAppState>,
    Query(query): Query<ConsentQuery>,
) -> Response {
    let txn = query.txn.unwrap_or_default();
    let now = now_unix();
    let provider = Arc::clone(&state.provider);
    let lookup_txn = txn.clone();
    let view = match run_blocking(move || provider.pending_consent(&lookup_txn, now)).await {
        Ok(v) => v,
        Err(panicked) => return panicked.into_response(),
    };
    let Some(view) = view else {
        return (StatusCode::BAD_REQUEST, Html(EXPIRED_HTML)).into_response();
    };
    let name = view.client_name.unwrap_or(view.client_id);
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Html(consent_html(&name, &view.redirect_uri, &txn)),
    )
        .into_response()
}

#[derive(Debug, Deserialize, Default)]
struct ConsentForm {
    txn: String,
    #[serde(default)]
    owner_token: String,
    #[serde(default)]
    action: String,
}

async fn consent_post(
    State(state): State<OAuthAppState>,
    Form(form): Form<ConsentForm>,
) -> Response {
    let now = now_unix();
    // Constant-time comparison only, no store I/O -- safe to run inline.
    let approved = form.action == "approve" && state.provider.verify_owner_token(&form.owner_token);

    let provider = Arc::clone(&state.provider);
    let txn = form.txn.clone();
    let outcome = match run_blocking(move || provider.decide_consent(&txn, approved, now)).await {
        Ok(v) => v,
        Err(panicked) => return panicked.into_response(),
    };
    match outcome {
        ConsentOutcome::Expired => (StatusCode::BAD_REQUEST, Html(EXPIRED_HTML)).into_response(),
        ConsentOutcome::Denied {
            redirect_uri,
            state,
        } => redirect_to(&construct_redirect_uri(
            &redirect_uri,
            &[
                ("error", Some("access_denied")),
                ("state", state.as_deref()),
            ],
        )),
        ConsentOutcome::Approved {
            code,
            redirect_uri,
            state,
        } => redirect_to(&construct_redirect_uri(
            &redirect_uri,
            &[("code", Some(code.as_str())), ("state", state.as_deref())],
        )),
    }
}

// ---------------------------------------------------------------------------
// /token
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenForm {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    scope: Option<String>,
}

fn token_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

fn token_success(tokens: super::types::TokenResponse) -> Response {
    (
        StatusCode::OK,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(tokens),
    )
        .into_response()
}

async fn token(State(state): State<OAuthAppState>, Form(form): Form<TokenForm>) -> Response {
    let now = now_unix();

    let Some(client_id) = form.client_id.clone() else {
        return token_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized_client",
            "Missing client_id",
        );
    };

    let provider = Arc::clone(&state.provider);
    let lookup_id = client_id.clone();
    let client = match run_blocking(move || provider.get_client(&lookup_id)).await {
        Ok(v) => v,
        Err(panicked) => return panicked.into_response(),
    };
    // `client.client_secret` is always `None` for a client this server ever
    // registers (see `Provider::register_client`), so there is no secret to
    // compare here -- see this module's doc for why that's not a shortcut.
    if client.is_none() {
        return token_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized_client",
            "Invalid client_id",
        );
    }

    match form.grant_type.as_str() {
        "authorization_code" => {
            let Some(code) = form.code.clone() else {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "code is required",
                );
            };
            let provider = Arc::clone(&state.provider);
            let (lookup_code, lookup_client) = (code.clone(), client_id.clone());
            let loaded = match run_blocking(move || {
                provider.load_authorization_code(&lookup_client, &lookup_code)
            })
            .await
            {
                Ok(v) => v,
                Err(panicked) => return panicked.into_response(),
            };
            let Some(loaded) = loaded else {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "authorization code does not exist",
                );
            };
            // RFC 6749 §10.5: expire codes after a deadline.
            if loaded.expires_at < now {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "authorization code has expired",
                );
            }
            // RFC 6749 §10.6: redirect_uri must not change between
            // /authorize and /token.
            let expected_redirect = loaded
                .redirect_uri_provided_explicitly
                .then(|| loaded.redirect_uri.clone());
            if form.redirect_uri != expected_redirect {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "redirect_uri did not match the one used when creating auth code",
                );
            }
            let Some(verifier) = form.code_verifier.clone() else {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "code_verifier is required",
                );
            };
            // RFC 7636 §4.6. A mismatch does NOT consume the code (matches
            // the reference: the code is only removed by a successful
            // exchange below).
            if !super::pkce::verify_pkce(&verifier, &loaded.code_challenge) {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "incorrect code_verifier",
                );
            }

            let provider = Arc::clone(&state.provider);
            let exchange_code = code.clone();
            let tokens = match run_blocking(move || {
                provider.exchange_authorization_code(&exchange_code, now)
            })
            .await
            {
                Ok(v) => v,
                Err(panicked) => return panicked.into_response(),
            };
            match tokens {
                Ok(Some(tokens)) => token_success(tokens),
                Ok(None) => token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "authorization code does not exist",
                ),
                // The code was valid and is now spent, but the tokens never
                // reached disk. 500, not 400: nothing the client sent was
                // wrong, and handing back a token the next request would
                // reject is the outcome issue #160 exists to prevent.
                Err(e) => {
                    eprintln!("oauth: could not persist issued tokens: {e}");
                    token_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "issued tokens could not be persisted",
                    )
                }
            }
        }
        "refresh_token" => {
            let Some(refresh_token) = form.refresh_token.clone() else {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "refresh_token is required",
                );
            };
            let provider = Arc::clone(&state.provider);
            let (lookup_token, lookup_client) = (refresh_token.clone(), client_id.clone());
            let loaded = match run_blocking(move || {
                provider.load_refresh_token(&lookup_client, &lookup_token, now)
            })
            .await
            {
                Ok(v) => v,
                Err(panicked) => return panicked.into_response(),
            };
            let Some(loaded) = loaded else {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "refresh token does not exist",
                );
            };
            let scopes: Vec<String> = match &form.scope {
                Some(requested) => requested.split(' ').map(str::to_string).collect(),
                None => loaded.scopes.clone(),
            };
            for scope in &scopes {
                if !loaded.scopes.contains(scope) {
                    return token_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_scope",
                        &format!("cannot request scope `{scope}` not provided by refresh token"),
                    );
                }
            }
            let provider = Arc::clone(&state.provider);
            let (exchange_client, exchange_token) = (client_id.clone(), refresh_token.clone());
            let tokens = match run_blocking(move || {
                provider.exchange_refresh_token(&exchange_client, &exchange_token, scopes, now)
            })
            .await
            {
                Ok(v) => v,
                Err(panicked) => return panicked.into_response(),
            };
            match tokens {
                Ok(tokens) => token_success(tokens),
                Err(e) => {
                    eprintln!("oauth: could not persist rotated tokens: {e}");
                    token_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "rotated tokens could not be persisted",
                    )
                }
            }
        }
        other => token_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            &format!("Unsupported grant type: {other}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// /register (RFC 7591)
// ---------------------------------------------------------------------------

async fn register(
    State(state): State<OAuthAppState>,
    Json(metadata): Json<ClientMetadata>,
) -> Response {
    let provider = Arc::clone(&state.provider);
    let result = match run_blocking(move || provider.register_client(metadata)).await {
        Ok(v) => v,
        Err(panicked) => return panicked.into_response(),
    };
    match result {
        Ok(info) => (StatusCode::CREATED, Json(info)).into_response(),
        Err(RegisterError::Invalid(err)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.error, "error_description": err.error_description })),
        )
            .into_response(),
        // A valid registration that could not be stored is a server fault.
        // Reporting it as invalid_client_metadata would send an integrator
        // to debug metadata that was never the problem (issue #160).
        Err(RegisterError::Storage(e)) => {
            eprintln!("oauth: could not persist client registration: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "server_error",
                    "error_description": "registration could not be persisted",
                })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// /revoke (RFC 7009)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RevokeForm {
    token: String,
    token_type_hint: Option<String>,
    client_id: String,
}

async fn revoke(State(state): State<OAuthAppState>, Form(form): Form<RevokeForm>) -> Response {
    let provider = Arc::clone(&state.provider);
    let lookup_id = form.client_id.clone();
    let client = match run_blocking(move || provider.get_client(&lookup_id)).await {
        Ok(v) => v,
        Err(panicked) => return panicked.into_response(),
    };
    if client.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(
                json!({ "error": "unauthorized_client", "error_description": "Invalid client_id" }),
            ),
        )
            .into_response();
    }

    let now = now_unix();
    let provider = Arc::clone(&state.provider);
    let (token_value, client_id, hint) = (
        form.token.clone(),
        form.client_id.clone(),
        form.token_type_hint.clone(),
    );
    let owning_client = match run_blocking(move || {
        // Try access first, then refresh, unless the hint says otherwise --
        // mirrors the reference's `loaders` list (and its `reversed()` for
        // token_type_hint == "refresh_token").
        let access = provider
            .load_access_token(&token_value, now)
            .map(|a| a.client_id);
        let refresh = provider
            .load_refresh_token(&client_id, &token_value, now)
            .map(|r| r.client_id);
        if hint.as_deref() == Some("refresh_token") {
            refresh.or(access)
        } else {
            access.or(refresh)
        }
    })
    .await
    {
        Ok(v) => v,
        Err(panicked) => return panicked.into_response(),
    };

    // If the token is not found, or belongs to a different client, this is
    // still a 200 per RFC 7009 §2.2 -- revocation never reveals whether a
    // token existed.
    if owning_client.as_deref() == Some(form.client_id.as_str()) {
        let provider = Arc::clone(&state.provider);
        let cid = form.client_id.clone();
        match run_blocking(move || provider.revoke_tokens_for_client(&cid)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                // RFC 7009 says a revocation endpoint returns 200 even for a
                // token it does not know -- but that is about unknown tokens,
                // not about a revocation this server failed to carry out.
                // Reporting success here would tell the caller a live token
                // is dead (issue #160).
                eprintln!("oauth: could not persist token revocation: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "server_error",
                        "error_description": "revocation could not be persisted",
                    })),
                )
                    .into_response();
            }
            Err(panicked) => return panicked.into_response(),
        }
    }

    (
        StatusCode::OK,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Bearer gate in front of /mcp (the reference's `RequireAuthMiddleware`)
// ---------------------------------------------------------------------------

fn unauthorized_bearer(state: &OAuthAppState) -> Response {
    let resource_metadata = resource_metadata_url(&state.issuer);
    let www_authenticate = format!(
        "Bearer error=\"invalid_token\", error_description=\"Authentication required\", resource_metadata=\"{resource_metadata}\""
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, www_authenticate.as_str())],
        Json(json!({ "error": "invalid_token", "error_description": "Authentication required" })),
    )
        .into_response()
}

/// Require a valid bearer token (an issued OAuth access token, or the
/// legacy connector token) before letting a request reach `/mcp`. Layered
/// only onto the `/mcp` route by `server::build_router` — every other OAuth
/// route authenticates itself differently (owner-credential consent,
/// client_id lookup, ...).
pub async fn require_bearer(
    State(state): State<OAuthAppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let header_value = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(token) = header_value.strip_prefix("Bearer ") else {
        return unauthorized_bearer(&state);
    };

    let now = now_unix();
    let provider = Arc::clone(&state.provider);
    let token_owned = token.to_string();
    let verified = match run_blocking(move || provider.load_access_token(&token_owned, now)).await {
        Ok(v) => v,
        Err(panicked) => return panicked.into_response(),
    };
    if verified.is_none() {
        return unauthorized_bearer(&state);
    }
    next.run(request).await
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// The OAuth authorization-server routes: metadata, `/authorize`,
/// `/consent`, `/token`, `/register`, `/revoke`. Does *not* include `/mcp`
/// itself — `server::build_router` mounts that separately, behind
/// [`require_bearer`].
pub fn oauth_router(state: OAuthAppState) -> Router {
    Router::new()
        .route(AS_METADATA_PATH, get(as_metadata))
        .route(AS_METADATA_ALIAS_PATH, get(as_metadata))
        .route(PR_METADATA_PATH, get(protected_resource_metadata))
        .route(PR_METADATA_ALIAS_PATH, get(protected_resource_metadata))
        .route(AUTHORIZATION_PATH, get(authorize_get).post(authorize_post))
        .route(TOKEN_PATH, post(token))
        .route(REGISTRATION_PATH, post(register))
        .route(REVOCATION_PATH, post(revoke))
        .route(CONSENT_PATH, get(consent_get).post(consent_post))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construct_redirect_uri_appends_to_a_bare_uri() {
        let out = construct_redirect_uri(
            "https://claude.ai/cb",
            &[("code", Some("abc")), ("state", Some("st4te"))],
        );
        assert_eq!(out, "https://claude.ai/cb?code=abc&state=st4te");
    }

    #[test]
    fn construct_redirect_uri_preserves_an_existing_query_string() {
        let out = construct_redirect_uri("https://claude.ai/cb?x=1", &[("code", Some("abc"))]);
        assert_eq!(out, "https://claude.ai/cb?x=1&code=abc");
    }

    #[test]
    fn construct_redirect_uri_drops_none_valued_params() {
        let out = construct_redirect_uri(
            "https://claude.ai/cb",
            &[("code", Some("abc")), ("state", None)],
        );
        assert_eq!(out, "https://claude.ai/cb?code=abc");
    }

    #[test]
    fn urlencode_component_percent_encodes_reserved_characters_and_plus_encodes_space() {
        assert_eq!(urlencode_component("a b"), "a+b");
        assert_eq!(urlencode_component("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn resource_metadata_url_matches_rfc_9728s_well_known_path_layout() {
        let issuer =
            super::super::issuer::validate_issuer("https://machine.tailnet.ts.net").unwrap();
        assert_eq!(
            resource_metadata_url(&issuer),
            "https://machine.tailnet.ts.net/.well-known/oauth-protected-resource/mcp"
        );
    }
}
