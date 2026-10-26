//! Router construction, the loopback-bind warning, and the process-level
//! entry points `remind_me_cli` calls into.

use std::net::SocketAddr;
use std::sync::Arc;

use remind_me_core::remote::RemoteConfig;
use remind_me_mcp::Handler;
use rusty_axum::middleware::from_fn;
use rusty_axum::routing::get;
use rusty_axum::{IntoResponse, Json, Request, Router, ServeOptions, Server, StatusCode};
use rusty_mcp_server::{HttpConfig, HttpHandler, Mount};
use rusty_tokio::io::TcpListener;
use serde_json::json;

use crate::auth::{secret_gate, GateConfig, HEALTH_PATH, MCP_PATH};
use crate::handler;
use crate::oauth::{self, IssuerError, OAuthAppState};

/// Bytes of reply kept so a dropped connection can resume (see
/// `rusty_mcp_server::HttpConfig::resume_buffer`).
const RESUME_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// The largest request body accepted, MCP or OAuth.
const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;

// A transcript at its limit must leave room for the JSON-RPC envelope.
const _: () =
    assert!(remind_me_core::transcript::MAX_TRANSCRIPT_BYTES + 512 * 1024 <= MAX_BODY_BYTES);

/// Why [`build_router`] failed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// The configured OAuth issuer is not acceptable.
    #[error(transparent)]
    Issuer(IssuerError),
    /// The MCP server could not be described from the handler.
    #[error(transparent)]
    Describe(#[from] handler::DescribeError),
}

/// The MCP endpoint as a `rusty_axum` handler.
fn mcp_mount(mcp: &Arc<dyn Handler>) -> Result<Mount, BuildError> {
    let server = handler::describe(Arc::clone(mcp))?;
    let http = Arc::new(HttpHandler::new(
        Arc::new(server),
        HttpConfig {
            path: MCP_PATH.to_owned(),
            // Any `Host`: behind a tunnel the public name is not knowable, and
            // the credential is the secret path / bearer token, not `Host`.
            allowed_hosts: Vec::new(),
            resume_buffer: RESUME_BUFFER_BYTES,
            ..HttpConfig::default()
        },
    ));
    // The gate has rewritten `/mcp/<token>` to `/mcp` (or a deeper path), so
    // every request is pinned to the one path the handler serves.
    Ok(Mount::new(http).at(MCP_PATH))
}

/// Whether `path` is `/mcp` or under it: what the MCP fallback serves.
fn is_mcp_path(path: &str) -> bool {
    path.strip_prefix(MCP_PATH)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

/// Whether `host` is loopback-only. Pure and free of any socket/env access
/// so the non-loopback-bind warning it drives is directly unit-testable
/// (`#85`'s acceptance criteria calls this out explicitly as an acceptable
/// alternative to asserting on captured log output).
pub fn is_loopback_host(host: &str) -> bool {
    LOOPBACK_HOSTS.contains(&host)
}

/// Warn to stderr when `host` isn't loopback — mirrors the reference's
/// `_warn_if_remote_host_widened` and its reasoning verbatim: this app never
/// terminates TLS, so a non-loopback bind with no tunnel in front puts the
/// secret-path/bearer token on the wire in plaintext to anything that can
/// reach `host:port`. Same as the reference, this is a warning rather than a
/// refusal — widening the bind is sometimes intentional (a tunnel that
/// forwards without proxying through loopback), and there is no reliable
/// way for the app itself to tell "arrived through the tunnel" from
/// "arrived directly" without trusting an attacker-influenced header.
pub fn warn_if_widened(host: &str, port: u16) {
    if !is_loopback_host(host) {
        eprintln!(
            "WARNING: remote MCP connector is binding to {host}:{port}, not loopback. \
             This is only safe with an HTTPS tunnel (or your own TLS termination) in \
             front of it -- the connector always speaks plain HTTP itself, so without a \
             tunnel every credential it accepts (the secret-path/bearer token) would \
             cross the wire in cleartext to anything that can reach {host}:{port} directly."
        );
    }
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// Build the full router: `GET /health` unauthenticated, `/mcp` (plus
/// `/mcp/<token>`) behind [`secret_gate`] — and, when `issuer` is `Some`,
/// the OAuth authorization server ([`oauth::oauth_router`]) mounted
/// alongside, with [`oauth::require_bearer`] guarding `/mcp` instead of the
/// legacy bearer check (the legacy secret-path/bearer token still works —
/// `secret_gate`'s OAuth-mode branch rewrites it into a bearer request; see
/// `auth.rs`'s module doc).
///
/// The MCP handler's `Host` allowlist (DNS-rebinding protection) is empty
/// here — in both modes — matching the reference's own
/// `TransportSecuritySettings(enable_dns_rebinding_protection=False)` and
/// its stated reasoning, which applies identically whether or not OAuth is
/// active: behind a tunnel the public hostname isn't knowable in advance,
/// and the actual credential is the secret path / bearer token (or an OAuth
/// access token, itself bound to the explicitly configured `issuer` — never
/// to `Host`), not the `Host` header.
///
/// Every request body, MCP or OAuth, is limited to the server's
/// `max_body_bytes` (see [`serve`]); a larger one is answered 413.
///
/// # Errors
///
/// Returns [`BuildError::Issuer`] if `issuer` is `Some` and fails
/// [`oauth::validate_issuer`] (not an https origin, or has a path/query/
/// fragment) — mirrors the reference's `build_remote_app`, which raises
/// `ValueError` synchronously at the same point for the same reason.
pub fn build_router(
    mcp: Arc<dyn Handler>,
    token: String,
    issuer: Option<String>,
) -> Result<Router, BuildError> {
    let mount = mcp_mount(&mcp)?;

    let Some(raw_issuer) = issuer else {
        let gate = Arc::new(GateConfig::legacy(token));
        let mcp = move |request: Request| {
            let mount = mount.clone();
            async move {
                if !is_mcp_path(request.path()) {
                    return StatusCode::NOT_FOUND.into_response();
                }
                mount.call(request).await
            }
        };
        return Ok(Router::new()
            .route(HEALTH_PATH, get(health))
            .fallback(mcp)
            .layer(gate_layer(gate)));
    };

    let issuer = oauth::validate_issuer(&raw_issuer).map_err(BuildError::Issuer)?;
    let store = remind_me_core::remote::OAuthStateStore::new(
        remind_me_core::remote::oauth_state_file_path(),
    );
    let provider = Arc::new(oauth::Provider::new(token.clone(), store));
    let oauth_state = OAuthAppState { provider, issuer };

    // The bearer check wraps only the MCP handler, not the OAuth routes,
    // which authenticate themselves differently (owner-credential consent,
    // client_id lookup, or nothing at all for public metadata).
    let bearer_state = oauth_state.clone();
    let mcp = move |request: Request| {
        let (state, mount) = (bearer_state.clone(), mount.clone());
        async move {
            if !is_mcp_path(request.path()) {
                return StatusCode::NOT_FOUND.into_response();
            }
            oauth::require_bearer(state, mount, request).await
        }
    };

    let gate = Arc::new(GateConfig {
        token,
        oauth_mode: true,
        extra_allow_paths: &[
            "/authorize",
            "/token",
            "/register",
            "/revoke",
            oauth::CONSENT_PATH,
        ],
        allow_prefixes: &["/.well-known/"],
    });

    Ok(Router::new()
        .route(HEALTH_PATH, get(health))
        .merge(oauth::oauth_router(oauth_state))
        .fallback(mcp)
        .layer(gate_layer(gate)))
}

/// [`secret_gate`] as a layer, in front of routing.
fn gate_layer(gate: Arc<GateConfig>) -> impl rusty_axum::middleware::Middleware {
    from_fn(move |request, next| secret_gate(Arc::clone(&gate), request, next))
}

/// Run the remote MCP connector until the process is killed. Binds
/// `config.host:config.port` and serves [`build_router`] with `rusty_axum`'s
/// HTTP/1.1 server. Must run on a `rusty_tokio` runtime.
///
/// This is the crate's sole public async entry point; [`run_blocking`]
/// (below) is what a synchronous caller like `remind_me_cli` actually uses.
pub async fn run(
    mcp: Arc<dyn Handler>,
    config: RemoteConfig,
    token: String,
) -> std::io::Result<()> {
    warn_if_widened(&config.host, config.port);
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let listener = TcpListener::bind(addr).await?;
    match &config.issuer {
        Some(issuer) => eprintln!(
            "Remote MCP connector listening on http://{addr}{MCP_PATH} with OAuth (FT-07) \
             ACTIVE -- issuer {issuer}. claude.ai can connect via OAuth discovery; the \
             legacy secret-path/bearer token keeps working too. Expose it via an HTTPS \
             tunnel (e.g. Tailscale Funnel) so the issuer's public origin actually reaches \
             this process."
        ),
        None => eprintln!(
            "Remote MCP connector listening on http://{addr}{MCP_PATH} (token redacted; \
             header-capable clients may instead send Authorization: Bearer <token> to \
             http://{addr}{MCP_PATH}). Expose it via an HTTPS tunnel (e.g. Tailscale Funnel) \
             and add the public /mcp/<token> URL as a claude.ai custom connector. Set \
             REMIND_ME_REMOTE_ISSUER to serve OAuth (FT-07) instead."
        ),
    }
    let router = build_router(mcp, token, config.issuer.clone())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let server = serve(listener, router)?;
    server.shutdown_on(std::future::pending()).await;
    Ok(())
}

/// Serve `router` on `listener` in the background, until the returned
/// [`Server`] is shut down. Every request carries its peer address, which
/// the secret gate keys its rate-limit bucket on (issue #920). Must be
/// called on a `rusty_tokio` runtime.
///
/// # Errors
///
/// When the listener cannot be served.
pub fn serve(listener: TcpListener, router: Router) -> std::io::Result<Server> {
    let options = ServeOptions {
        max_body_bytes: MAX_BODY_BYTES,
        ..ServeOptions::default()
    };
    rusty_axum::serve(listener, None, options, router)
}

/// Synchronous entry point for `remind_me_cli`: owns spinning up a
/// dedicated multi-thread `rusty_tokio` runtime, resolves the bind config and
/// connector token from the environment
/// (`remind_me_core::remote::{RemoteConfig::from_env, resolve_connector_token}`),
/// and blocks on [`run`].
///
/// Kept in this crate rather than the CLI so the async boundary `#57`'s
/// decision drew around `remind_me_remote` stays exactly that — one crate —
/// instead of leaking an `async fn` or a runtime dependency into
/// `remind_me_cli`, which stays synchronous like every other crate in this
/// workspace.
pub fn run_blocking(mcp: Arc<dyn Handler>) -> std::io::Result<()> {
    let config = RemoteConfig::from_env();
    let token = remind_me_core::remote::resolve_connector_token();
    let runtime = rusty_tokio::Builder::new_multi_thread()
        .thread_name("remind-me-remote")
        .build()?;
    runtime.block_on(run(mcp, config, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_recognised() {
        for host in ["127.0.0.1", "localhost", "::1"] {
            assert!(is_loopback_host(host), "{host} should be loopback");
        }
    }

    #[test]
    fn non_loopback_hosts_are_not_loopback() {
        for host in ["0.0.0.0", "192.168.1.5", "example.com", ""] {
            assert!(!is_loopback_host(host), "{host} should not be loopback");
        }
    }
}
