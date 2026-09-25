# ADR-0010: Remote MCP transport — the `rmcp` SDK behind a thin `ServerHandler` adapter, in its own async crate

Status: Accepted
Date: 2026-07-30

## Context

`remind_me_mcp/remote.py` exposes the FastMCP server as a remote connector
over MCP's Streamable HTTP transport, gated by a secret URL path
(`/mcp/<token>`) or an `Authorization: Bearer <token>` header, so claude.ai
(Settings → Connectors → Add custom connector) can attach to it through an
HTTPS tunnel. Present in the reference, missing here (`#85`, split off the
transport epic on `#57`). OAuth 2.1 (`remote.py`'s FT-07 branch) and
`remind_me_revoke_clients` are a separate, blocked-on-this issue (`#86`) —
out of scope for this ADR.

`#57`'s decision comment already settled the headline question — a new,
isolated `remind_me_remote` crate on `tokio`/`axum`, not a retrofit of the
synchronous crates and not a hand-rolled `std::net` implementation of
Streamable HTTP's session-managed SSE framing — and named `rmcp` (the
official Rust MCP SDK) as the crate to investigate for the transport itself.
This ADR records what that investigation actually found, since `rmcp`'s real
API shape is materially different from a naive "point it at a JSON-RPC
handler" assumption, and the design had to follow what was found rather than
the other way around.

## Investigation: what `rmcp` 3.0.1 actually provides

Confirmed reachable via `cargo add --dry-run`, then added for real and its
source read directly from
`~/.cargo/registry/src/index.crates.io-.../rmcp-3.0.1/` — not taken on faith
from its crate description. Two findings shaped every module in
`remind_me_remote`:

1. **No raw JSON-RPC passthrough mode exists.**
   `transport::streamable_http_server::tower::StreamableHttpService<S, M>` —
   the type that actually implements Streamable HTTP's session-managed SSE
   over a `tower_service::Service` axum can mount — is generic over
   `S: rmcp::ServerHandler`, the SDK's own typed trait
   (`list_tools`, `call_tool`, `read_resource`, `list_prompts`, `initialize`,
   ...). There is no constructor taking a bare
   `fn(&str) -> Option<serde_json::Value>`. The SDK owns protocol-level
   dispatch itself (`handler/server.rs`'s blanket
   `impl<H: ServerHandler> Service<RoleServer> for H`): it decodes every
   request into a typed `ClientRequest` variant, negotiates protocol
   version, and calls the matching `ServerHandler` method.
2. **`StreamableHttpServerConfig` defaults to a Host-header allowlist**
   (`localhost`/`127.0.0.1`/`::1`) — DNS-rebinding protection that would
   reject every request arriving through a tunnel with a public hostname,
   confirmed empirically (see below) before being disabled.

## Decision

**A thin `ServerHandler` adapter (`handler::RemindMeHandler`), not a
reimplementation of tool/resource/prompt dispatch.** For each `ServerHandler`
method the stdio transport's own dispatch already answers (`tools/list`,
`tools/call`, `resources/list`, `resources/read`, `prompts/list`), the
adapter builds the identical JSON-RPC envelope
`remind_me_mcp::McpServer::run_stdio_loop` sends, calls
`McpServer::handle_request` — the crate's one, already-tested dispatch entry
point — on `tokio::task::spawn_blocking` (so a slow tool call holding
`Database`'s connection mutex can't stall the tokio runtime), and
deserializes the `result` field straight into the matching `rmcp::model`
type via `serde_json::from_value`. That last step needed no hand-written
field mapping: `handle_request`'s existing JSON shapes
(`{"tools": [...]}`, `{"content": [...], "isError": ...}`,
`{"contents": [...]}`) already match rmcp's own camelCase wire format for
`ListToolsResult`, `CallToolResult`, `ReadResourceResult`, etc. — a
coincidence of both sides implementing the same MCP spec, not something
this crate had to engineer. `initialize` uses rmcp's own default
implementation (built on a `get_info()` override) rather than a hand-rolled
call into `handle_request`'s `initialize` branch, since rmcp's default does
real protocol-version negotiation the stdio transport's own simpler
echo-back does not need to match.

Methods `handle_request` doesn't answer either (`prompts/get`,
subscriptions, tasks, completion) are left at rmcp's own default
(`method not found` / an empty result) — matching, not exceeding, the stdio
transport's coverage.

**`StreamableHttpServerConfig::default().disable_allowed_hosts()`** in
`server::build_router`, matching the reference's own
`TransportSecuritySettings(enable_dns_rebinding_protection=False)` and its
stated reasoning verbatim: behind a tunnel the public hostname isn't
knowable in advance, and the actual credential is the secret-path/bearer
token, not the Host header.

**Auth as axum middleware wrapping the whole router** (`auth::secret_gate`),
not baked into rmcp's own request handling — a direct, minimal port of the
reference's `SecretPathMiddleware`: `/health` passes through
unauthenticated; `/mcp/<token>` is rewritten to `/mcp` (constant-time
compared, via `remind_me_core::webhook::constant_time_eq` — reused, not
reimplemented) before reaching the inner router; `/mcp` with the wrong or
missing bearer is 401; anything else is 404, identically whether it's a
wrong token segment or a wholly unrelated path.

**Token generation and status reporting live in `remind_me_core`**
(`remote.rs`), not in `remind_me_remote` itself, even though the token is
only ever *used* by this crate's async server. This mirrors
`webhook::WebhookStatus`/`sync::worker::SyncWorkerStatus`: `remind_me_mcp`'s
`remind_me_server_status` tool needs to report the connector's
enabled/host/port/token-configured state, and must not pull
tokio/axum/rmcp into an otherwise entirely synchronous crate just to read an
env var and stat a file. Splitting "state a sync caller can report" from
"the async server that produces it" is exactly the boundary those two
existing status types already draw. Token generation reuses the `uuid`
crate's `v4` feature (backed by `getrandom`, an OS-entropy source) — two
concatenated v4 UUIDs — rather than adding a `rand` dependency for one call
site; see `remote.rs`'s module doc for why that differs from
`remind_me_api`'s explicit choice *not* to invent a random source for
`resolve_api_key` (that key has an unauthenticated fallback; this token does
not, since it doubles as the connector URL's secret path segment).

## Consequences

- `tokio`, `axum`, and `rmcp` are dependencies of `remind_me_remote` only.
  `remind_me_core`, `remind_me_api`, `remind_me_mcp`, and `remind_me_cli`
  remain synchronous and unchanged architecturally; `remind_me_cli` gained
  one new `remote` subcommand that calls `remind_me_remote::run_blocking`
  (which owns its own tokio runtime) rather than gaining any `async fn` of
  its own.
- Unit tests cannot exercise `RemindMeHandler`'s `ServerHandler` methods
  directly: each takes a `RequestContext<RoleServer>`, whose `Peer<R>` field
  has a `pub(crate)`-only constructor inside `rmcp`. Coverage for those
  methods instead lives in `tests/http_test.rs`, driving the real
  `StreamableHttpService` over HTTP with `reqwest` — session negotiation,
  SSE framing, and the secret-path/bearer auth gate are all exercised
  end-to-end rather than through a hand-built `RequestContext`, which is a
  stronger test in any case.
- This crate's test suite (unit tests plus the HTTP integration tests) can
  assert protocol shape and that this crate's own auth/routing logic is
  correct. It cannot prove interop with an actual claude.ai custom
  connector — this sandboxed environment has no network path to reach one.
  That remains an explicit open item, recorded in `RELEASE_NOTES.md`, for a
  human to validate before `#85` is considered fully done.

## Alternatives considered

**Hand-rolling Streamable HTTP's SSE/session framing on `std::net`**, this
workspace's consistent choice everywhere else. Rejected for the reason
`#57`'s decision comment already gives: a subtly wrong implementation of
session-managed SSE would pass every test in this repo while still failing
silently against a real claude.ai connector, which this environment cannot
validate against — exactly the failure mode a from-spec reimplementation
risks most when the SDK doing the same job is both official and available.

**Building the transport around the reference's actual wire behavior
without reading `rmcp`'s source**, taking its documented API at face value.
Rejected in favor of vendoring and reading the crate directly (and, once the
adapter compiled, empirically probing a real request/response cycle with a
throwaway client) — `rmcp` 3.0.1 turned out to expose a typed trait rather
than a raw dispatch hook, which a docs-only read could easily have missed
or guessed wrong about.
