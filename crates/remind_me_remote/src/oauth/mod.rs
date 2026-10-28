//! The OAuth 2.1 authorization server (`#86`), as a `rusty_axum` adapter
//! over the shared, framework-free `rusty_oauth_server` crate.
//!
//! remind-me has one user, so the consent page takes the connector token
//! (the "owner credential") and every grant acts as [`OWNER`]. The same
//! token keeps working as a bearer on `/mcp`.
//!
//! The provider does small, synchronous file I/O, so each request runs on a
//! blocking thread, as `handler.rs` does for MCP calls.

use std::sync::Arc;

use rusty_axum::routing::any;
use rusty_axum::{IntoResponse, Json, Request, Response, Router, State, StatusCode};
use rusty_crypto_key::constant_time_eq;
use rusty_mcp_server::Mount;
use rusty_oauth_server::{
    now_unix, Branding, Endpoints, Method, Provider, StateStore, Subject, Verify, PATHS,
};
use serde_json::json;

pub use rusty_oauth_server::{validate_issuer, Issuer, IssuerError, CONSENT_PATH};

/// The one subject every remind-me grant acts as.
pub const OWNER: &str = "owner";

const BRANDING: Branding = Branding {
    resource_name: "remind_me",
    resource_description: "your remind_me memory store",
    token_label: "Owner token (the remote connector token)",
};

/// Shared state for the OAuth routes and the `/mcp` bearer gate.
#[derive(Clone)]
pub struct OAuthAppState {
    endpoints: Arc<Endpoints>,
}

impl OAuthAppState {
    /// The OAuth state for `issuer`: consent approved by `owner_token`,
    /// grants kept in `store`.
    pub fn new(owner_token: String, issuer: Issuer, store: StateStore) -> Self {
        let verify: Verify = Box::new(move |presented| {
            constant_time_eq(presented.as_bytes(), owner_token.as_bytes())
                .then(|| Subject::new(OWNER))
        });
        let provider = Arc::new(Provider::new(verify, store));
        Self {
            endpoints: Arc::new(Endpoints::new(provider, issuer, BRANDING)),
        }
    }
}

/// Run `f` on a blocking thread; a panic there is a plain 500.
async fn run_blocking<T, F>(f: F) -> Result<T, Response>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    rusty_tokio::spawn_blocking(f).await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "server_error" })),
        )
            .into_response()
    })
}

/// A shared-crate response as a `rusty_axum` one.
fn to_axum(response: rusty_oauth_server::Response) -> Response {
    let status = StatusCode::from_u16(response.status);
    response
        .headers
        .iter()
        .fold(Response::new(status, response.body), |r, (name, value)| {
            r.with_header(name, value)
        })
}

async fn handle(State(state): State<OAuthAppState>, request: Request) -> Response {
    let method = match request.method() {
        rusty_axum::Method::Get => Method::Get,
        rusty_axum::Method::Post => Method::Post,
        _ => Method::Other,
    };
    let path = request.path().to_string();
    let query = request.query().unwrap_or_default().to_string();
    let body = request.body().to_vec();
    let answered = run_blocking(move || {
        let request = rusty_oauth_server::Request {
            method,
            path: &path,
            query: &query,
            body: &body,
        };
        state.endpoints.handle(&request, now_unix())
    })
    .await;
    match answered {
        Ok(Some(response)) => to_axum(response),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(panicked) => panicked,
    }
}

/// Require a valid bearer (an issued access token, or the connector token)
/// before a request reaches `mcp`. Wraps only `/mcp`; the OAuth routes
/// authenticate themselves.
pub async fn require_bearer(state: OAuthAppState, mcp: Mount, request: Request) -> Response {
    let header = request.headers().get("authorization").unwrap_or("");
    let Some(token) = rusty_oauth::bearer::token_from_authorization(header) else {
        return to_axum(state.endpoints.unauthorized());
    };
    let token = token.to_string();
    let endpoints = Arc::clone(&state.endpoints);
    let verified = run_blocking(move || {
        let provider = endpoints.provider();
        provider.verify(&token).is_some()
            || provider.load_access_token(&token, now_unix()).is_some()
    })
    .await;
    match verified {
        Ok(true) => mcp.call(request).await,
        Ok(false) => to_axum(state.endpoints.unauthorized()),
        Err(panicked) => panicked,
    }
}

/// The OAuth routes: metadata, `/authorize`, `/consent`, `/token`,
/// `/register`, `/revoke`. `/mcp` is mounted separately, behind
/// [`require_bearer`].
pub fn oauth_router(state: OAuthAppState) -> Router {
    PATHS
        .iter()
        .fold(Router::new(), |router, path| {
            router.route(path, any(handle))
        })
        .with_state(state)
}
