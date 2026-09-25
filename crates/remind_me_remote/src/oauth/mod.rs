//! Single-user OAuth 2.1 authorization server (`#86`, the FT-07 slice of
//! the transport epic on `#57`). Ports `remind_me_mcp/oauth.py` and
//! `remote.py`'s OAuth-mode branch (`build_remote_app`'s `if not issuer:`
//! *else* clause) — PKCE (S256) authorization-code flow with refresh, RFC
//! 8414 AS metadata, RFC 9728 protected-resource metadata, RFC 7591 dynamic
//! client registration, RFC 7009 revocation, and the `/consent` owner-token
//! approval page.
//!
//! # No SDK to lean on
//!
//! The Python reference builds this almost entirely from
//! `mcp.server.auth` (`create_auth_routes`, `create_protected_resource_routes`,
//! `RequireAuthMiddleware`, ...) — the installed MCP SDK's own OAuth
//! authorization-server framework, reading its actual installed source
//! (`mcp.server.auth.{routes,provider,settings}` and
//! `mcp.server.auth.handlers.*`, `mcp.server.auth.middleware.*`) to confirm
//! its exact validation order, error codes, and response shapes rather than
//! inferring them from field names. `rmcp` 3.0.1 (this crate's Rust MCP
//! SDK) has no equivalent: its own `auth` feature (confirmed by reading
//! `transport/auth.rs` and `Cargo.toml` directly, the same way `#85`'s ADR
//! investigated `StreamableHttpService`) is *client*-side OAuth for
//! connecting to someone else's OAuth-protected MCP server (built on the
//! `oauth2` crate) — nothing implements the server side. So this module is
//! hand-rolled from the reference's actual behavior and the RFCs it cites,
//! not adapted from an existing Rust auth framework. See this crate's ADR
//! for the full investigation and the "hand-roll vs. new dependency"
//! decision it records.
//!
//! # Module layout
//!
//! - [`issuer`] — `REMIND_ME_REMOTE_ISSUER` validation (https origin, no
//!   path/query/fragment; localhost may be http).
//! - [`pkce`] — RFC 7636 S256 `code_challenge` computation/verification.
//! - [`types`] — RFC 7591 client registration/record wire types and the
//!   RFC 6749 §5.1 token response.
//! - [`provider`] — the policy layer: consent, issuance, refresh rotation,
//!   revocation, over [`remind_me_core::remote::OAuthStateStore`].
//! - [`routes`] — the axum routes and the bearer-auth gate in front of
//!   `/mcp`.
//!
//! # No new dependency
//!
//! PKCE's SHA-256 reuses the workspace's existing `sha256` dependency;
//! base64url and the issuer/redirect-URI parsing this module needs are
//! hand-rolled rather than adding `base64`/`url` crates for a handful of
//! call sites each — the same "don't add a dependency for one call site"
//! precedent `remind_me_core::remote`'s token-generation doc already
//! records for this workspace. Token generation itself reuses
//! `remind_me_core::remote::generate_token` (now `pub`) rather than
//! duplicating that reasoning next to a second implementation.

pub mod issuer;
pub mod pkce;
pub mod provider;
pub mod routes;
pub mod types;

pub use issuer::{validate_issuer, Issuer, IssuerError};
pub use provider::{Provider, CONSENT_PATH};
pub use routes::{oauth_router, require_bearer, resource_metadata_url, OAuthAppState};
