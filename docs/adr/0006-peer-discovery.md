# ADR-0006: Peer discovery — static peer list plus Tailscale's local API

Status: Accepted
Date: 2026-07-30

## Context

The third sync slice, continuing ADR-0004/ADR-0005's hub-and-graph-table
sync onto multiple remotes. The issue's own notes flag Tailscale as "an
external dependency worth an explicit decision" — reading `sync.py`'s
"Peer discovery" section directly (there is no separate `tailscale.py`)
before writing any of this settled what that dependency actually is:

- **No Tailscale client library, no shelling out to the `tailscale` CLI.**
  It's a plain `GET /localapi/v0/status` over Tailscale's own local API —
  reached over a Unix domain socket
  (`/var/run/tailscaled.socket` on macOS, `/var/run/tailscale/tailscaled.sock`
  elsewhere, overridable via `REMIND_ME_TAILSCALE_SOCKET`), parsed as plain
  JSON (`Peer: {name: {Online, TailscaleIPs, HostName}}`).
- **No tag/hostname-pattern filter at discovery time.** Every `Online`
  peer with at least one address is provisionally a candidate, addressed at
  `http://{first_ip}:{PEER_PORT}`. The *actual* "is this a remind_me
  instance" check happens later and uniformly, for both Tailscale-sourced
  and static peers alike: probing `/health` with the sync secret right
  before syncing, reusing the exact same check a hub already gets.
- **The reference's own test suite never touches a real `tailscaled`.**
  Every test fakes the transport. This means the feature was always
  designed to be verifiable without the real daemon present — confirmed
  by reading the test file, not assumed — which is exactly the same shape
  this port needs: fake the Unix socket server, not the daemon.
- **`REMIND_ME_STATIC_PEERS`** is a JSON array of `{"node_id": ...,
  "url": ...}` objects — confirmed the exact shape from both the parsing
  code and its test fixtures, not guessed. Static peers seed a
  by-URL dedup set *before* Tailscale peers are added, so a static entry
  wins a URL collision (confirmed directly against
  `test_discover_peers_parses_tailscale_status`'s `static-dup` case, which
  a Tailscale-sourced duplicate must not shadow).

## Decision

**A hand-rolled Unix-socket HTTP client**, `sync::peers`'s `unix_socket`
submodule: `std::os::unix::net::UnixStream`, one `GET
/localapi/v0/status` request, the same minimal HTTP/1.1-over-a-raw-stream
approach every other hand-rolled client in this crate already uses
(`embedder.rs`'s Ollama client, `sync::http`'s hub client) — no new
dependency for a single request/response shape.

**Gated behind `#[cfg(unix)]`, with a `Vec::new()` stub elsewhere.**
Tailscale's local API is only ever reached over a Unix domain socket; a
non-Unix build has no equivalent to fall back to, and degrades to "no
Tailscale peers" — precisely the same observable outcome a Unix machine
gets when Tailscale itself isn't installed (a socket-connect failure).
Static peers are unaffected either way — they need no socket at all.

**No filter beyond `Online` + non-empty address at discovery time,
reusing the existing `probe_peer` health check** (the same shape as the
hub gets no different treatment) as the sole "is this really a remind_me
peer" gate, applied uniformly right before syncing with any discovered
peer, static or Tailscale-sourced. Building a second, discovery-time
filter (a tag convention, a hostname pattern) would be inventing a
convention the reference doesn't have.

**Static peers processed first, so they win a URL collision** — matches
the reference's own dedup order exactly, verified against its own test
case rather than assumed to be either order.

**One deliberate divergence: a malformed `REMIND_ME_STATIC_PEERS` *value*
degrades to an empty list instead of crashing the process.** The
reference's own `config.py` does `json.loads(...)` with no exception
handling at import time — a genuinely broken env var value there takes
the whole server down before it can even report why. Every other optional
feature in this crate (the webhook secret, the watch-dirs list, the hub
URL) degrades gracefully when misconfigured; this one deviation in the
reference reads as an oversight, not a considered design choice, so this
port does not reproduce it. A malformed *individual entry* within an
otherwise-valid array is still skipped, not the whole list — that part
does match the reference exactly.

## Alternatives considered

**A real `tailscale status --json` CLI shell-out instead of the local API
socket.** Rejected: the reference doesn't do this (confirmed by reading
`sync.py`, not assumed), and shelling out would add a process-spawn
dependency and PATH assumption this crate has consistently avoided
elsewhere (`ADR-0002`, `ADR-0003`) in favor of the more auditable, more
testable direct-protocol approach — which here is a plain HTTP GET over a
socket path, not a subprocess.

**Filtering discovered Tailscale peers by tag/ACL/hostname convention
before probing.** Rejected: the reference has no such filter — the
`/health` probe *is* the entire membership test, applied identically to
every candidate regardless of source. Inventing a stricter discovery-time
filter would be adding a security-relevant behavior the reference doesn't
have and this port has no basis to design correctly.

## Consequences

- A node with Tailscale running and other tailnet peers running
  `rusty_remind_me` (or the real `remind_me`, since the wire protocol is
  shared) syncs with all of them automatically, no configuration beyond
  the three existing sync env vars — Tailscale peers need no
  `REMIND_ME_STATIC_PEERS` entry at all.
- A node without Tailscale (or on a non-Unix platform) still gets full
  sync functionality via the hub and any statically configured peers —
  peer discovery is additive, never a prerequisite.
- Every sync cycle now potentially talks to more than one remote; a slow
  or unreachable peer only costs that peer's own probe timeout (3s) before
  the cycle moves on — matching the reference's own per-peer, non-blocking
  error handling.
- This still does not implement OAuth or `remind_me_revoke_clients` — the
  last remaining pieces of the sync epic, each its own follow-up.
