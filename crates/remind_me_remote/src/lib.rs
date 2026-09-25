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
//! # Why this is its own crate, on tokio + axum + rmcp
//!
//! Recorded as a binding decision on `#57` before this issue was filed (see
//! that issue's decision comment) and restated in `#85`'s acceptance
//! criteria; summarized here because it shapes every module in this crate:
//!
//! - MCP's Streamable HTTP transport is session-managed SSE with
//!   resumability, not simple JSON-RPC-over-HTTP — hand-rolling that framing
//!   on `std::net` (this workspace's consistent choice everywhere else, e.g.
//!   `remind_me_api`/`remind_me_core::webhook`) risks a subtly wrong
//!   implementation that passes every test in this repo while still failing
//!   against a real claude.ai connector, which this environment cannot
//!   validate against directly.
//! - `tokio` has sat declared-but-unused in the workspace `Cargo.toml` since
//!   before this epic started. Scoping it, `axum`, and `rmcp` to this one
//!   crate keeps `remind_me_core`/`remind_me_api`/`remind_me_mcp`/
//!   `remind_me_cli` untouched and synchronous — this stays additive, not an
//!   architecture-wide async migration forced by one feature.
//!
//! # rmcp's actual API shape (investigated, not assumed)
//!
//! `rmcp = "3.0.1"` was confirmed reachable via `cargo add --dry-run` and
//! then actually vendored locally (`~/.cargo/registry/src/.../rmcp-3.0.1/`)
//! and read directly — not taken on faith from its crates.io description.
//! Two findings that shaped this crate's design:
//!
//! 1. **No raw JSON-RPC passthrough mode.** `transport::streamable_http_server`'s
//!    `StreamableHttpService<S, M>` requires `S: rmcp::ServerHandler`, the
//!    SDK's typed trait (`list_tools`, `call_tool`, `read_resource`, ...) —
//!    there is no constructor that takes a bare `fn(&str) -> Option<Value>`.
//!    So [`handler::RemindMeHandler`] is the thin adapter this crate's
//!    original design doc anticipated might be necessary: it implements the
//!    handful of `ServerHandler` methods `McpServer::handle_request` already
//!    answers by building the same JSON-RPC envelope the stdio transport
//!    sends and calling that one existing, already-tested dispatch entry
//!    point — no tool/resource/prompt logic is reimplemented here. See
//!    `handler.rs`'s module doc for the rest of that reasoning, including
//!    why the JSON `handle_request` already returns needs no hand-written
//!    field mapping into rmcp's typed results.
//! 2. **`StreamableHttpServerConfig` defaults to a Host-header allowlist**
//!    (DNS-rebinding protection) that would reject every request arriving
//!    through a tunnel with a public hostname. [`server::build_router`]
//!    disables it, matching the reference's own
//!    `enable_dns_rebinding_protection=False` and its stated reasoning
//!    (the actual credential is the secret-path/bearer token, not the Host
//!    header).
//!
//! # The async boundary
//!
//! [`McpServer`](remind_me_mcp::McpServer) itself stays synchronous —
//! nothing in `remind_me_mcp`/`remind_me_core` changed to accommodate this
//! crate. `handler::RemindMeHandler::dispatch` is the one place sync and
//! async meet: it hands `handle_request` calls to
//! `tokio::task::spawn_blocking` rather than calling them inline, so a slow
//! tool call (holding `Database`'s connection mutex) cannot stall the tokio
//! runtime's other async work. `Database`'s `Mutex<rusqlite::Connection>`
//! (already `Send + Sync` before this crate existed — `remind_me_api`
//! already serves it from its own thread) is what makes sharing one
//! `Arc<McpServer>` across concurrent connector sessions sound at all; see
//! `handler.rs`'s `remind_me_handler_is_send_and_sync` test, which asserts
//! that compile-time property directly rather than assuming it holds.
//!
//! This crate's own test suite (unit tests here, plus `tests/http_test.rs`
//! driving the real axum/rmcp server over HTTP) can assert protocol shape —
//! that `initialize` negotiates, `tools/call` round-trips through
//! `handle_request`, auth gates the right paths, SSE framing looks like what
//! rmcp itself produces. It cannot prove interop with an actual claude.ai
//! custom connector, which this sandboxed environment has no network path
//! to reach. That remains an explicit open item; see `RELEASE_NOTES.md`.
//!
//! # SEP-2567 (protocol version >= 2026-07-28): stateless per-request dispatch
//!
//! `rmcp` 3.0.1 implements both the classic session-managed lifecycle every
//! client up to `2025-11-25` uses (`legacy_session_mode`, kept on here —
//! that's what a real-world client like `mcp-remote` still speaks, and
//! there is no reason to drop support for it) and SEP-2567's newer
//! "discover lifecycle": a `2026-07-28`+ client can call a tool in a single
//! POST with no prior `initialize`/session at all, deciding per-request
//! rather than per-connection. `tower.rs`'s `handle_post` already routes to
//! whichever lifecycle a given request's protocol version calls for — nothing
//! in `handler.rs`'s `RemindMeHandler` needed to change for that, since
//! `dispatch` was already per-call and stateless. The one piece this crate
//! does add is [`event_store::InProcessEventStore`], wired into
//! `server::build_router`'s `LocalSessionManager`: it's what lets `GET /mcp`
//! (resuming a dropped stream via `Last-Event-Id`) work for both lifecycles
//! at once, rather than only the legacy session-based one. See that
//! module's doc for why.

pub mod auth;
pub mod event_store;
pub mod handler;
pub mod oauth;
pub mod server;

pub use event_store::InProcessEventStore;
pub use handler::RemindMeHandler;
pub use server::{build_router, is_loopback_host, run, run_blocking, warn_if_widened};
