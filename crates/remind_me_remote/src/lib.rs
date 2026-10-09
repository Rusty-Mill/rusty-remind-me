//! Remote MCP connector over Streamable HTTP. `#85` ported the FT-05
//! (no-OAuth) slice of the transport epic recorded on `#57`:
//! `remind_me_mcp/remote.py`'s legacy/secret-path mode (`SecretPathMiddleware`
//! and `build_remote_app`'s `if not issuer:` branch), `get_remote_status`.
//! `#86` (this crate's [`oauth`] module) adds FT-07: the single-user OAuth
//! 2.1 authorization server (`build_remote_app`'s OAuth-mode branch) and
//! `remind_me_revoke_clients` (registered in `remind_me_mcp`, operating on
//! `remind_me_core::remote::OAuthStateStore`). The two modes share one
//! router (`server::build_router`) and the legacy secret-path/bearer
//! credential keeps working in both.
//!
//! # Why this is its own crate, on tokio + axum
//!
//! Recorded as a binding decision on `#57`: scoping `tokio` and `axum` to this
//! one crate keeps `remind_me_core`/`remind_me_api`/`remind_me_mcp`/
//! `remind_me_cli` untouched and synchronous. The MCP protocol itself is the
//! workspace's first-party stack: [`handler::describe`] turns the synchronous
//! [`remind_me_mcp::Handler`] into a `rusty_mcp_server` server (it used to be
//! an `rmcp` `ServerHandler` adapter), and `rusty_mcp_axum` mounts that
//! server's HTTP handler in the axum router, so the auth gate and the OAuth
//! routes stay ordinary axum.
//!
//! # The async boundary
//!
//! [`McpServer`](remind_me_mcp::McpServer) itself stays synchronous. The MCP
//! handlers are blocking too, and the HTTP layer runs each request on a
//! blocking thread, so a slow tool call (holding `Database`'s store lock)
//! cannot stall the tokio runtime's other work. `Database` being
//! `Send + Sync` is what makes sharing one `Arc<McpServer>` across
//! concurrent connector sessions sound.
//!
//! # Resumable replies
//!
//! Every answer is an event stream that opens with a priming event, and
//! carries ids, so a client behind a tunnel that drops mid-reply reconnects
//! with `GET /mcp` and `Last-Event-Id` (no session needed, which is what
//! `2026-07-28` clients do) and gets what it missed. See
//! `rusty_mcp_server::HttpConfig::resume_buffer`.
//!
//! This crate's tests (`tests/http_test.rs`, `tests/oauth_test.rs`) drive the
//! real server over HTTP. They cannot prove interop with an actual claude.ai
//! custom connector, which this sandboxed environment has no network path
//! to reach. That remains an explicit open item; see `RELEASE_NOTES.md`.

pub mod auth;
pub mod handler;
pub mod oauth;
pub mod server;

pub use server::{build_router, is_loopback_host, run, run_blocking, warn_if_widened, BuildError};
