//! Router construction, the loopback-bind warning, and the process-level
//! entry points `remind_me_cli` calls into.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use remind_me_core::remote::RemoteConfig;
use remind_me_mcp::McpServer;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;

use crate::auth::{secret_gate, GateConfig, HEALTH_PATH, MCP_PATH};
use crate::event_store::InProcessEventStore;
use crate::handler::RemindMeHandler;
use crate::oauth::{self, IssuerError, OAuthAppState};

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
/// `rmcp`'s own `StreamableHttpServerConfig` defaults to a Host-header
/// allowlist of `localhost`/`127.0.0.1`/`::1` (DNS-rebinding protection).
/// That default is disabled here — in both modes — matching the
/// reference's own `TransportSecuritySettings(enable_dns_rebinding_protection=False)`
/// and its stated reasoning, which applies identically whether or not OAuth
/// is active: behind a tunnel the public hostname isn't knowable in
/// advance, and the actual credential is the secret path / bearer token (or
/// an OAuth access token, itself bound to the explicitly configured
/// `issuer` — never to `Host`), not the `Host` header. Enforcing the
/// default allowlist would just break every tunneled connection while
/// adding no protection this app doesn't already have from those checks.
///
/// `session_manager` also carries an [`InProcessEventStore`] (see that
/// module's doc, and `lib.rs`'s SEP-2567 section): `legacy_session_mode`
/// stays on (default) so `mcp-remote` and every other pre-`2026-07-28`
/// client keeps the session-managed lifecycle it already speaks, while the
/// event store is what makes `rmcp` also serve `GET /mcp`'s resumable
/// stream for `2026-07-28`+ clients using the newer, session-free discover
/// lifecycle — `tower.rs`'s own `supports_stateless_replay` check. Nothing
/// else here changes per lifecycle: `RemindMeHandler::dispatch` was already
/// stateless per call, and `tower.rs` picks the right branch per request
/// based on its negotiated protocol version.
///
/// # Errors
///
/// Returns [`IssuerError`] if `issuer` is `Some` and fails
/// [`oauth::validate_issuer`] (not an https origin, or has a path/query/
/// fragment) — mirrors the reference's `build_remote_app`, which raises
/// `ValueError` synchronously at the same point for the same reason.
pub fn build_router(
    mcp: Arc<McpServer>,
    token: String,
    issuer: Option<String>,
) -> Result<Router, IssuerError> {
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    let session_manager = Arc::new(
        LocalSessionManager::default().with_event_store(Arc::new(InProcessEventStore::new())),
    );
    let service: StreamableHttpService<RemindMeHandler, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(RemindMeHandler::new(Arc::clone(&mcp))),
            session_manager,
            config,
        );

    let Some(raw_issuer) = issuer else {
        let gate = Arc::new(GateConfig::legacy(token));
        return Ok(Router::new()
            .route(HEALTH_PATH, get(health))
            .nest_service(MCP_PATH, service)
            .layer(middleware::from_fn_with_state(gate, secret_gate)));
    };

    let issuer = oauth::validate_issuer(&raw_issuer)?;
    let store = remind_me_core::remote::OAuthStateStore::new(
        remind_me_core::remote::oauth_state_file_path(),
    );
    let provider = Arc::new(oauth::Provider::new(token.clone(), store));
    let oauth_state = OAuthAppState { provider, issuer };

    // `route_layer` (as opposed to `layer`) applies only to the routes
    // already registered on *this* sub-router -- i.e. only `/mcp`, not the
    // OAuth routes merged in below, which authenticate themselves
    // differently (owner-credential consent, client_id lookup, or nothing
    // at all for public metadata).
    let mcp_router =
        Router::new()
            .nest_service(MCP_PATH, service)
            .route_layer(middleware::from_fn_with_state(
                oauth_state.clone(),
                oauth::require_bearer,
            ));

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
        .merge(mcp_router)
        .layer(middleware::from_fn_with_state(gate, secret_gate)))
}

/// Run the remote MCP connector until the process is killed or the listener
/// fails. Binds `config.host:config.port` and serves [`build_router`] with
/// axum's own HTTP/1.1 + h2 server.
///
/// This is the crate's sole public async entry point; [`run_blocking`]
/// (below) is what a synchronous caller like `remind_me_cli` actually uses.
pub async fn run(mcp: Arc<McpServer>, config: RemoteConfig, token: String) -> std::io::Result<()> {
    warn_if_widened(&config.host, config.port);
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
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
    axum::serve(listener, router).await
}

/// Synchronous entry point for `remind_me_cli`: owns spinning up a
/// dedicated multi-thread tokio runtime, resolves the bind config and
/// connector token from the environment
/// (`remind_me_core::remote::{RemoteConfig::from_env, resolve_connector_token}`),
/// and blocks on [`run`].
///
/// Kept in this crate rather than the CLI so the tokio/axum/rmcp async
/// boundary `#57`'s decision drew around `remind_me_remote` stays exactly
/// that — one crate — instead of leaking an `async fn` or a tokio
/// dependency into `remind_me_cli`, which stays synchronous like every
/// other crate in this workspace.
pub fn run_blocking(mcp: McpServer) -> std::io::Result<()> {
    let config = RemoteConfig::from_env();
    let token = remind_me_core::remote::resolve_connector_token();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(Arc::new(mcp), config, token))
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
