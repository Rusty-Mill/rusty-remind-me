# ADR-0036: A capture-only connector credential

Status: Proposed (2026-11-23); D64 to D67; D140 (Hermes)
Date: 2026-11-23

Continues ADR-0029, whose cloud hook needs a credential it can carry.

## Context

ADR-0029's cloud hook sends a session's transcript to the remote server with
the connector token, set as an environment variable by an environment's setup
script. Cloud sessions started from Claude Duet and claude.ai Projects get
neither: no environment variables of the user's, no setup script. Claude Code
in those sessions also skips plugin `mcp_tool` hooks, so a hook cannot go
through the remind-me connector either. Plugin `command` hooks from plugins
uploaded to claude.ai do run.

A plugin is files, so its credential is readable by every agent in every
container it is installed in, including a prompt-injected one. The connector
token can read, change and delete every memory; it cannot go there.

The Hermes memory provider (ADR-0028 step 6, `plugins/hermes-memory/`) has
the same need remotely: one capture per session, replaced on a repeated
callback, with no verified client credential of its own. It uses this
credential too when it is set, and falls back to `remind_me_auto_capture`
without it (D140). Nothing on the server changes for it: the gate, the
profile exemption and the `remind-me-capture-token` stamp below apply as
they are.

## Decision

1. **A second, optional credential: `REMIND_ME_REMOTE_CAPTURE_TOKEN`.** Env
   only, never generated or persisted. It resolves to Off (unset), Unusable
   (blank, under 32 characters, anything but visible ASCII, or equal to the
   connector token; the server runs and capture-only access stays closed) or
   Ready. `remind_me_server_status` reports the state as
   `remote.capture_token`, never the value.
2. **It can only save a transcript.** `secret_gate` accepts it as
   `Authorization: Bearer` on `POST /mcp` only (no secret-path form), for a
   single JSON-RPC 2.0 message that is `initialize`,
   `notifications/initialized`, `ping`, or `tools/call` of
   `remind_me_capture_transcript` inside a session (`Mcp-Session-Id`).
   `Mcp-Method` and `Mcp-Name` headers must agree with the body. Anything
   else is 403 `{"error":"Forbidden"}`. It has its own rate-limit bucket.
3. **What was checked is what is read.** The gate forwards the parsed
   message re-serialised, not the bytes it received, so duplicate keys
   cannot read one way at the gate and another behind it. It then swaps the
   `Authorization` header for the connector token, so the legacy check and
   OAuth's `require_bearer` run unchanged in both modes.
4. **Its writes are marked.** On `initialize` the gate sets
   `clientInfo.name` to `remind-me-capture-token`, so every write of that
   session carries that `client`. How the daily routine should treat them is
   open (Q23).
5. **It works under any tool profile.** `remind_me_capture_transcript` is in
   the `import` group, outside `core`. A call to it whose caller is
   `remind-me-capture-token` passes the profile check under any profile; it
   stays out of `tools/list` wherever the profile hides it, and every other
   tool, aliases included, keeps the check (D68). This is the first
   per-caller profile slice (ADR-0028, ADR-0030), and for this credential it
   replaces ADR-0029's note that a server taking hook captures runs `full`.
   Widening the profile instead would put the import tools in every
   connector chat.
6. **The hook lives here.** `plugins/cloud-capture/` is a private-plugin
   template: Stop and PreCompact `command` hooks running the capture script,
   which reads the URL and token from a `config.json` beside it (gitignored)
   or, as before, from the environment. It is not in the public marketplace.
   rusty_mill's copy is removed in its own PR (D66).

## Consequences

- A leaked capture token lets its holder write transcripts, and nothing else.
  Rotating it is one environment variable and a restart.
- A capture-token request that reuses another session's `Mcp-Session-Id`
  inherits that session's identity. Session ids are random and never shown,
  so this is accepted (D67).
- A connector-token client that names itself `remind-me-capture-token`
  reaches the capture tool under a narrow profile too. It already holds full
  access, so this grants it one write tool it could otherwise get by widening
  its profile.
