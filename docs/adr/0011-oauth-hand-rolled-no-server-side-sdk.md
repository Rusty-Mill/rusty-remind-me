# ADR-0011: OAuth 2.1 authorization server — hand-rolled from the RFCs, no server-side SDK available

Status: Accepted
Date: 2026-07-30

## Context

`remind_me_mcp/oauth.py` implements a single-user OAuth 2.1 authorization
server (FT-07) — RFC 8414 AS metadata, RFC 9728 protected-resource
metadata, RFC 7591 dynamic client registration, a PKCE (S256, mandatory)
authorization-code flow with refresh, and RFC 7009 revocation — almost
entirely on top of the installed Python MCP SDK's `mcp.server.auth`
package (`create_auth_routes`, `create_protected_resource_routes`,
`RequireAuthMiddleware`, `ClientAuthenticator`, ...). `remote.py`'s
`build_remote_app` mounts it alongside the FT-05 secret-path connector
when `REMIND_ME_REMOTE_ISSUER` is set. Present in the reference, missing
here (`#86`, blocked on `#85`'s transport crate existing at all, split off
the epic on `#57`). `remind_me_revoke_clients` (`tools/admin.py`) is the
MCP-tool half of the same slice.

`#57`'s decision comment (restated in `#86`'s own text) named this as
security-sensitive code needing the same rigor as the OTel/OTLP hand-roll
in `#79`, not a best-effort approximation, and flagged the specific risk
the issue's own notes call out explicitly: guessing at
`remind_me_revoke_clients(client_id="")`'s semantics from the parameter
name rather than reading `oauth.py`.

## Investigation: is there a Rust equivalent to `mcp.server.auth`?

`rmcp = "3.0.1"` (this workspace's MCP SDK, already vendored for `#85`) was
read directly from `~/.cargo/registry/src/.../rmcp-3.0.1/`, the same way
`#85`'s ADR investigated `StreamableHttpService` — not assumed from the
crate's description. Its `auth` Cargo feature (`transport/auth.rs`,
`transport/common/auth.rs`) exists, but is **client**-side OAuth: it
lets an `rmcp` client authenticate itself *against* someone else's
OAuth-protected MCP server, built on the `oauth2` crate (PKCE, discovery,
token exchange, refresh — all as a *client*). There is no
`mcp.server.auth`-equivalent authorization-*server* framework anywhere in
the crate — no route builders, no provider trait, no bearer-auth
middleware for protecting routes `rmcp` itself serves. Confirmed by
reading every file the `auth`/`auth-client-credentials-jwt` Cargo features
gate, not just the feature list.

This matches what `#86`'s own issue text anticipated ("there's no
off-the-shelf Rust equivalent... this needs the same rigor as the
OTel/OTLP work in #79, not a best-effort approximation") rather than
contradicting it — recorded here because the alternative (a well-scoped
third-party OAuth *server* crate) was seriously considered and rejected:
no actively-maintained Rust crate implements "OAuth 2.1 authorization
server as an axum-mountable router" as a general-purpose library the way
Python's `mcp.server.auth` is one; the closest candidates are either
full IdP servers (wrong shape, far too much surface for a single-user
in-process AS) or low-level token/JWT libraries that would still leave
almost all of `oauth.py`'s actual logic — the RFC 7591 registration
handler, the PKCE-verifying token handler, the consent flow, the client
registry — to hand-write regardless. Pulling in a heavy dependency to
save writing the metadata JSON and a handful of route handlers was not a
good trade for a single-user server with the exact semantics `oauth.py`
already pins down precisely.

## Decision

**Hand-roll the OAuth 2.1 authorization server from the reference's actual
behavior and the RFCs it cites**, verified line-by-line against
`remind_me_mcp/oauth.py`, `remote.py`'s OAuth-mode branch, and (critically)
the *installed* Python MCP SDK source under
`.venv/lib/python3.11/site-packages/mcp/server/auth/` — not the SDK's
public docs, which don't specify things like the exact issuer-validation
rule (`scheme == "https" or host in ("localhost",) or host.startswith("127.0.0.1")`,
read from `routes.py`'s `validate_issuer_url`) or the precise PKCE-mismatch-
does-not-consume-the-code behavior (read from `handlers/token.py`).

New module, `crates/remind_me_remote/src/oauth/`:

- `issuer.rs` — `validate_issuer`, a small hand-rolled origin parser (no
  `url` crate: this workspace has none, and the surface needed — scheme /
  authority / path / query / fragment, no userinfo, no percent-decoding —
  is narrow enough that a dedicated parser is simpler to audit than a
  general-purpose one). Combines the SDK's `validate_issuer_url` rule with
  `remote.py`'s own additional path-must-be-root check, exactly as
  `build_remote_app` applies both together.
- `pkce.rs` — RFC 7636 S256: `sha256::digest` (already a workspace
  dependency) plus a hand-rolled hex-decode and base64url-encode (no
  `base64` crate for one 32-byte encode), verified against RFC 7636
  Appendix B's own test vector.
- `types.rs` — RFC 7591 client registration/record wire types and the
  RFC 6749 §5.1 token response, typed with `serde`; ad hoc metadata/error
  responses stay as `serde_json::json!` literals in `routes.rs`, matching
  this workspace's existing convention (`remind_me_mcp` builds every tool
  response the same way) rather than a struct per one-shot shape.
- `provider.rs` — the policy layer the reference's `SingleUserOAuthProvider`
  is: consent (parked, process-local `HashMap`s — codes and pending
  consents, exactly like the reference's own reasoning for why those don't
  need to survive a restart), issuance/refresh/revocation over
  `remind_me_core::remote::OAuthStateStore`.
- `routes.rs` — the axum routes themselves and the bearer-auth gate in
  front of `/mcp` (the reference's `RequireAuthMiddleware`).

**A deliberate simplification, called out in `routes.rs`'s own module doc**:
since `Provider::register_client` always forces `token_endpoint_auth_method
= "none"` and `client_secret = None` (mirroring the reference's own
`register_client`, whose lengthy doc comment explains exactly why — PKCE
already provides proof of possession, and there's no secret to compare
without also being unable to prove Ohashed-at-rest secret against a
plaintext-in request), the reference's general-purpose `ClientAuthenticator`
(which *can* verify `client_secret_basic`/`_post`) reduces, for every
client this server can ever register, to "client_id must name a registered
client." `/token` and `/revoke` implement exactly that reduction rather
than porting the general-purpose authenticator, since the secret-comparison
branch is provably unreachable here — not a narrowing of what the reference
actually does for this provider, just not generalized beyond it.

**`OAuthStateStore` lives in `remind_me_core::remote`, not
`remind_me_remote`** — mirroring `RemoteConfig`/`RemoteStatus`'s existing
split (documented in `remote.rs`'s own module doc) between "state a sync
caller can read/mutate" and "the async server that serves it."
`remind_me_mcp`'s `remind_me_revoke_clients` tool (sync, no tokio) needs to
list and revoke OAuth clients the same way the reference's tool does
(`asyncio.to_thread(store.list_clients)` in a sync-at-heart MCP tool
handler) — `remind_me_remote`'s async `Provider` wraps the same store type
rather than owning a second one.

## `client_id=""` semantics — verified, not assumed

`#86`'s own text warned against guessing this from the parameter name.
Read directly from `tools/admin.py`'s `remind_me_revoke_clients`: an empty
`client_id` **lists** every registered client (with live token counts);
there is no "revoke all" operation at all — the only way to revoke
multiple clients is to call the tool once per `client_id` from that
listing. A non-empty `client_id` revokes exactly that one client's
registration and every token it holds; an unknown `client_id` is an
error, not a silent no-op or an accidental "revoke everything that
matched." `crates/remind_me_mcp/src/lib.rs`'s `remind_me_revoke_clients`
case ports this exactly, and `crates/remind_me_remote/tests/oauth_test.rs`'s
`revoke_clients_semantics_empty_id_lists_nonempty_id_revokes_one_not_all`
asserts it end to end against a live server (register two clients, confirm
empty `client_id` lists both untouched, confirm a specific `client_id`
revokes only that one, confirm an unknown `client_id` is `None`/error).

## DNS-rebinding protection: unchanged reasoning, now applies to OAuth too

`#85`'s `server::build_router` already disables `rmcp`'s Host-header
allowlist (`StreamableHttpServerConfig::default().disable_allowed_hosts()`),
matching the reference's `enable_dns_rebinding_protection=False` and its
stated reasoning: behind a tunnel the public hostname isn't knowable in
advance, and the actual credential is the secret-path/bearer token, not
`Host`. OAuth mode doesn't change that reasoning, it extends it: the
issuer is explicit, operator-configured `REMIND_ME_REMOTE_ISSUER`
(validated by `oauth::validate_issuer`), never derived from the inbound
`Host` header, and an OAuth access token is bound to that issuer, not to
whatever hostname a request happened to arrive on. `build_router`'s doc
comment says this explicitly rather than leaving it implicit.

## Legacy coexistence

`auth::secret_gate` (the reference's `SecretPathMiddleware`) gained a
`GateConfig` (`oauth_mode`, `extra_allow_paths`, `allow_prefixes`) so one
middleware serves both modes: in OAuth mode, `/mcp/<token>` is still
rewritten to `/mcp`, additionally injecting `Authorization: Bearer <token>`
so `oauth::require_bearer` (layered only onto the `/mcp` route via
`route_layer`, not `layer`, so it never gates the OAuth routes themselves)
authenticates it the same way it authenticates an issued OAuth access
token — `Provider::load_access_token` checks the legacy connector token
(constant-time) before falling through to a store lookup. This is a direct
port of the reference's own "re-express the secret path as a bearer
credential" comment in `SecretPathMiddleware`.

## A real, environment-specific bug found and fixed along the way

`OAuthStateStore`'s read-modify-write cycle (read whole JSON file, mutate
one entry, write whole file back) intermittently lost data under this
crate's own test suite running with real parallelism (`cargo test`'s
default many-OS-threads-in-one-process model): a `fs::write` that returned
`Ok(())` was, under heavy concurrent filesystem activity in this sandboxed
environment, occasionally not yet visible to an immediately-following
`fs::read_to_string` in the *same* process — sometimes even within the same
function, a few lines later. Plain `fs::write` was replaced with an
explicit `File::create` + `write_all` + `sync_all` (`write_and_sync`), and
`write`/`read` both gained bounded retry with short backoff (`write`
verifies its own write by reading it back before returning; `read` retries
on a miss only once this store has successfully written at least once,
so a genuinely-fresh, never-written store still reads a missing file as
empty immediately, matching the reference). This was reproduced
reliably (∼15–50% failure rate across dozens of full-suite runs before the
fix, 0 failures across 90+ runs after), root-caused with targeted
`eprintln!` instrumentation (since removed) rather than guessed at, and is
unrelated to any of this crate's own locking (`OAuthStateStore`'s
`Mutex<()>` already serializes this process's own operations correctly —
the observed staleness was a same-thread, sequential read not seeing its
own immediately-preceding write, not a concurrency bug in this crate's
code). Whether this is specific to this sandboxed environment's storage
layer or a broader latent risk, the fix is strictly more correct
regardless: a state file that silently drops a just-issued token under any
amount of filesystem contention would be a serious, hard-to-diagnose bug
in a real deployment too.

## Consequences

- No new third-party dependency (`sha256` was already a workspace
  dependency; it's now also a direct dependency of `remind_me_remote`).
  `reqwest`'s `form`/`query` Cargo features were enabled on the existing
  `dev-dependencies` entry for integration tests — not a new dependency,
  an existing one gaining features it didn't need before OAuth's
  form-encoded routes existed.
- `remind_me_core`/`remind_me_mcp`/`remind_me_api`/`remind_me_cli` stay
  synchronous; only `remind_me_remote` gained `oauth/` (tokio/axum, as
  already decided for the whole crate on `#57`/`#85`).
- The hand-rolled surface (issuer validation, PKCE, the route handlers) is
  covered by unit tests per module plus `tests/oauth_test.rs`, an
  HTTP-level integration suite in the same style as `#85`'s
  `tests/http_test.rs` — both exercising `build_router` exactly as
  production uses it, not a mocked subset.
- Not yet validated against a real claude.ai custom connector (OAuth
  discovery + consent + token flow) — this sandboxed environment has no
  network path to do that, same open item `#85`'s ADR already recorded for
  the transport half. Explicitly still required before merge per `#86`'s
  own acceptance checklist.
