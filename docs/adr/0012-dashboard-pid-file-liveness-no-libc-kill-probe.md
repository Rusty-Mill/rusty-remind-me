# ADR-0012: Dashboard PID-file liveness — health check only, no `libc::kill(0)` pre-filter

Status: Accepted
Date: 2026-07-30

## Context

`#90` ports the reference's `remind_me_mcp/pid.py`: a JSON PID file written
by the dashboard process on start, read cross-process by the MCP server to
answer `remind_me_server_status`, and used by the dashboard's own start-up
to refuse a double start.

The reference's `_read_pid_file()` does two checks in sequence before
`get_server_status()` trusts a PID file:

1. `os.kill(pid, 0)` — signal 0, no-op if the process exists, `OSError` if
   it doesn't. A cheap, local, no-network way to discard an obviously-dead
   PID file before ever touching the network.
2. `_check_ui_server_health()` — `GET {url}/health`, 2s timeout. The actual
   liveness proof: a `kill(0)` success only means *some* process holds that
   PID, not that it is this dashboard, still bound to that port, still
   answering.

`get_server_status()`'s final decision is `info and _check_ui_server_health(...)`
— the health check is authoritative either way. `kill(0)` is purely an
optimization that skips a network round-trip against a process that
plainly no longer exists.

Porting `kill(0)` faithfully needs a raw syscall binding. This workspace
has no `libc` dependency anywhere — every other hand-rolled protocol client
here (`sync/http.rs`, `embedder.rs`'s `OllamaEmbedder`, the webhook and API
servers) uses only `std::net`/`std::process`, and adding `libc` for one
`kill(pid, 0)` call would be the first FFI dependency in the crate for a
check the reference itself treats as an optimization, not the source of
truth.

## Decision

**Skip the `kill(0)` pre-filter. Rely on the `GET /health` probe alone** as
both the staleness check and the liveness proof, in
`crates/remind_me_core/src/pid.rs`'s `dashboard_status`.

Every case the pre-filter exists to shortcut still resolves correctly
through the health check by itself:

- **Dead process, PID file left behind** (crashed, `kill -9`'d, or any
  path that skips the graceful-shutdown cleanup): the health check fails
  (nothing is listening, or the connection is refused/times out), the file
  is removed, `running: false`. The only difference from the reference is
  that this path always makes one TCP connection attempt to a dead
  address instead of short-circuiting on `kill(0)` — a local connection
  failure is fast, not a real network round-trip, so the cost this
  optimization was avoiding barely applies to `127.0.0.1` in the first
  place.
- **Live process, wrong service now on that port** (PID reused by an
  unrelated process, or a non-dashboard process bound to the recorded
  port): `kill(0)` would succeed and *wrongly* skip further checking in a
  literal port of the reference's ordering, if the reference didn't also
  always run the health check regardless. Since both implementations
  actually gate on the health check either way, this case is handled
  identically by both.
- **Live, healthy dashboard**: identical outcome in both — one HTTP round
  trip either way, since the reference makes it too.

No case changes correctness; the only cost is one extra TCP connection
attempt against a dead loopback address in the crash-cleanup case, which is
fast enough that CI or an operator would never notice it in practice.

## Consequences

- No new dependency for `#90`; `remind_me_core` stays `libc`-free.
- `pid::dashboard_status`'s staleness signal is "didn't answer `/health`"
  rather than "the OS says the PID is gone" — a stale file is still
  cleaned up automatically on the next status check either way, so this is
  an implementation-detail difference, not a behavior gap an operator or
  the reference's own test suite would observe.
- If a genuine no-network staleness pre-filter becomes worth it later (e.g.
  a very hot status-polling path against a normally-dead dashboard), adding
  `libc` for exactly that `kill(0)` call is a small, self-contained,
  revisit-able change — this ADR is the record of why it wasn't needed at
  `#90`'s scope, not a decision that it can never be added.
