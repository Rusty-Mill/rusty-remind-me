# ADR-0008: Serve the dashboard by vendoring `App.jsx` verbatim; CORS matches the reference exactly

Status: Accepted
Date: 2026-07-30

## Context

`remind_me_mcp/api.py`'s `_build_dashboard_html()` (`GET /`) and
`remind_me_mcp/dashboard/App.jsx` were present in the reference and missing
here — found unfiled during a parity sweep (`#78`). This crate's own
`RELEASE_NOTES.md` (the `#48` HTTP API entry) already stated the gap
outright: *"CORS is not implemented: nothing in this crate serves the
dashboard HTML the reference's CORS policy exists to protect."* No
follow-up issue was ever filed for it.

Reading `api.py` directly (not assumed) settled two things before writing
any code:

- **`App.jsx` is a client-side-only React component.** It talks to
  `window.location.origin + "/api"` via `fetch`, with a bearer key read from
  `localStorage`. Nothing in it is Python-specific — it doesn't care what
  language serves the JSON it fetches. `#48` already ships the `/api/*`
  routes it calls.
- **`_build_dashboard_html()` embeds `App.jsx` verbatim** into a
  `<script type="text/babel">` block inside a static HTML wrapper that
  loads React/ReactDOM/Babel from `unpkg.com`, pinned to exact versions with
  Subresource Integrity hashes. No build step, no bundler — the reference's
  own deliberate simplicity choice (stated in its own comment: "the
  dashboard still requires network access to unpkg.com on first load", i.e.
  it does not work offline, by design).
- **`Route("/", index)` is registered in the *same* Starlette app and
  middleware list as every `/api/*` route** — including the same
  `CORSMiddleware` (`allow_origin_regex=r"http://(localhost|127\.0\.0\.1)(:\d+)?"`,
  `allow_methods=["*"]`, `allow_headers=["*"]`, confirmed from the actual
  middleware configuration, not assumed). Dashboard and API share one origin
  in the primary use case; CORS exists for the cases where they don't (a
  separate dev server, a different port).

## Decision

**Vendor `App.jsx` verbatim** into `crates/remind_me_api/src/dashboard/App.jsx`
— a straight copy, not a rewrite, the same convention `schema_*.sql`
already established for the generated schema: this crate's own copy is
"regenerate by re-copying from the reference," not a file to hand-edit.
Since the JSX is backend-agnostic, this needed zero adaptation to run
against this crate's own `/api/*` routes.

**Reproduce `_build_dashboard_html()`'s HTML wrapper exactly** — the same
CDN URLs, the same pinned versions, the same SRI hashes, embedded via
`include_str!` for the JSX. `GET /` is registered in the same `ROUTES` table
as every `/api/*` route, so it shares the same CORS handling and sits
outside `check_auth`'s `/api/`-prefixed scope (unauthenticated, matching the
reference: serving the page itself needs no key, only the data it fetches
does).

**A hand-rolled CORS origin matcher** (`http::cors_allowed_origin`), not a
new `regex` dependency: the reference's policy is one fixed pattern (an
optional port on `localhost` or `127.0.0.1`, `http://` only), simple enough
to match directly without pulling in a general-purpose regex engine for one
use site. Applied to **every** response this server sends — not just
`/api/*` — via a new `write_response_cors` that threads an optional
reflected origin through, matching Starlette's `CORSMiddleware` wrapping
the whole app rather than one route group.

**`OPTIONS` is answered uniformly, before routing or auth.** A CORS
preflight never carries the eventual request's credentials and never
reaches a real handler in the reference either; answering it first (200,
with CORS headers if the origin matches, none if it doesn't) avoids
teaching every route handler about a method it never otherwise needs to
know exists.

**`sidecars.py` (a Windows Job Object keeping an SSH tunnel and,
optionally, a separate dashboard-UI process alive) is out of scope for this
issue.** It's a different concern — platform-specific process supervision
for *another* process, not serving the dashboard HTML this issue is about —
and driven from the sync loop in the reference, which makes `#57` a more
natural home for deciding it, if it's ported at all. Not silently dropped:
recorded here as an explicit scope decision, not an oversight.

## Alternatives considered

**Rewriting the dashboard against a Rust-side templating/bundling
pipeline.** Rejected: the reference's own architecture is deliberately
build-step-free (CDN-loaded React, in-browser Babel), a considered
simplicity choice worth preserving rather than "upgrading" unrequested.
Vendoring the exact file that already works is more faithful, not less.

**A `regex` crate dependency for the CORS origin check.** Rejected: one
fixed, simple pattern doesn't justify a general-purpose regex engine — this
crate has consistently avoided new dependencies for narrowly-scoped parsing
needs elsewhere (URL parsing in `sync/http.rs`, pattern matching in
`http::match_pattern`).

**Porting `sidecars.py` alongside this.** Rejected for this issue
specifically: it isn't what "serve the dashboard" requires, it's
Windows-specific, and it's driven from a sync cycle this branch has no
sync worker to hook into yet. Deciding "not here" explicitly is what the
issue's own acceptance criteria asked for — a decision, not a default.

## Consequences

- A browser opening `http://<host>:<port>/` gets the same dashboard the
  reference serves, calling this crate's own `/api/*` routes, with no
  adaptation needed on the JSX side.
- CORS now applies uniformly to every response this server sends, matching
  the reference's middleware-wraps-the-whole-app behavior — including error
  responses (a 404, a 401), which a browser tab still needs the header on
  to read at all.
- The dashboard still requires network access to `unpkg.com` on first load,
  exactly like the reference — an inherited limitation, not a regression.
  (Verified live: the route serves correct HTML end to end, and the page
  loads and reaches `#root` with zero errors when the sandbox this was
  built in has direct network access; the three CDN `<script>` fetches are
  the only requests that fail when it doesn't, which is expected and
  matches the reference's own stated offline limitation, not a defect in
  this port.)
- `sidecars.py`'s scope remains an open, explicitly-recorded question for
  whoever picks up `#57` next, rather than a silent gap.
