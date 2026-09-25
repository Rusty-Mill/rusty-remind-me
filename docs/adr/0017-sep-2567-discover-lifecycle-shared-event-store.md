# ADR-0017: SEP-2567 discover lifecycle — an attached `EventStore`, not a mode switch

Status: Accepted
Date: 2026-08-09

## Context

ADR-0010 recorded `remind_me_remote`'s original transport decision: `rmcp`
3.0.1's `StreamableHttpService`, wrapping `RemindMeHandler` behind
`legacy_session_mode` (the default) — every client negotiates `initialize`
into a persistent, `Mcp-Session-Id`-keyed session, then reuses it for every
later `tools/list`/`tools/call`/etc.

`rmcp` 3.0.1 (the same version, no upgrade) also implements SEP-2567's newer
"discover lifecycle": a client at protocol version `2026-07-28`+ can call a
tool in a single POST with no `initialize`, no session, and no
`Mcp-Session-Id` at all. `tower.rs`'s own `is_legacy_request`/
`uses_legacy_lifecycle` already decide, per request, from the negotiated
protocol version, which lifecycle a given request gets — this was true
before this ADR and needed no code change to keep being true. What was
missing, and is what this ADR is actually about, is narrower: `GET /mcp`
(the resumable SSE stream, resumed via `Last-Event-Id`) only works when the
`SessionManager` reports an attached `EventStore`
(`tower.rs`'s `supports_stateless_replay` check) — `build_router` never
attached one, so `GET` degraded to `405 Method Not Allowed` for every
non-legacy request, and legacy-session resumption fell back to a more
limited in-worker mechanism instead of the store-backed one `rmcp` prefers
when available (`LocalSessionManager::resume`, read directly rather than
assumed: it consults `self.event_store` first, for both lifecycles, before
ever falling back to the worker-channel resume path).

`2026-07-28`-negotiated request dispatch itself needed nothing from this
crate: `RemindMeHandler::dispatch` (ADR-0010) was already a stateless
per-call function reused from `McpServer::handle_request` — `tower.rs`'s
discover-lifecycle branch calls the same `get_service()` factory the legacy
branch does and drives it through `serve_directly_with_ct` with no session
manager involvement at all.

## Decision

**Attach an `EventStore` to `LocalSessionManager`; do not touch
`legacy_session_mode`.** `rmcp` ships no `EventStore` implementation, only
the trait (`store_event`/`replay_events_after`) — `event_store.rs`'s
`InProcessEventStore` is a direct, minimal implementation of that contract:
a capped in-memory ring buffer keyed by a `u64` counter, filtering
`replay_events_after` to the same `stream_id` the caller is asking to
resume. Single-process, matching every other `remind_me_remote`/
`remind_me_core::remote` design choice (one connector, one `Database`
mutex, no distributed state) — `rmcp`'s separate `SessionStore` trait
(cross-instance session recovery, for horizontal scaling) is a different
concern this crate has never needed and still doesn't.

This is additive, not a lifecycle switch. `legacy_session_mode` stays on
(the default): `mcp-remote` and every other pre-`2026-07-28` client keeps
the exact session-managed lifecycle ADR-0010 built, byte-for-byte. The
event store is what makes `GET /mcp` newly capable for **both** lifecycles
at once — SEP-2567 clients get resumable per-request response streams they
had no path to before, and legacy sessions get the store-backed resume
`LocalSessionManager` already preferred, instead of the fallback.

## Consequences

- `crates/remind_me_remote/Cargo.toml` gains `async-trait` (already a
  workspace dependency, used by `remind_me_core`/`remind_me_mcp`; promoted
  here rather than newly vetted) for the `EventStore` trait's async
  methods, and `futures` (already present transitively via `rmcp` itself;
  promoted to a direct dependency for `futures::stream::{empty, iter}`).
- The one thing this ADR's decision could not exercise directly: a
  discover-lifecycle response that streams multiple events over time (long
  tool call, incremental notifications) and gets dropped mid-stream.
  `RemindMeHandler` never produces more than one response message per call,
  so every persisted stream in this crate's own test suite has exactly two
  events (the priming/retry event, then the one response) — enough to prove
  `replay_events_after` genuinely round-trips through the real router
  (`tests/http_test.rs`), not enough to prove behavior under a stream with
  three or more events. Nothing in `InProcessEventStore`'s own logic treats
  two specially, but this remains unexercised by anything short of a
  synthetic unit test (`event_store.rs`'s own `#[cfg(test)]` module, which
  does cover multi-event streams directly against the store).
- Tracked as `#246` (filed after implementation, not before — this ADR and
  its `RELEASE_NOTES.md` entry cite it rather than a number invented ahead
  of a real issue).

## Alternatives considered

**Flip `legacy_session_mode` off**, serving only the discover lifecycle.
Rejected outright: `mcp-remote` and Claude Desktop — real, currently
connected clients — speak the session-managed lifecycle and nothing else.
Dropping it would be a breaking change for the only clients this connector
demonstrably works with today, in exchange for a lifecycle no verified
client of this project uses yet.

**A `SessionStore` (cross-instance) instead of, or in addition to, an
`EventStore`.** Solves a different problem — a second server process
picking up a session the first one created — that `remind_me_remote` does
not have: it is explicitly single-process, single-connector, matching
`remind_me_core::remote::OAuthStateStore`'s own stated design (a shared
JSON file read on every check, not a distributed store). Adding one would
be speculative generality for a topology this crate has no plan to run in.
