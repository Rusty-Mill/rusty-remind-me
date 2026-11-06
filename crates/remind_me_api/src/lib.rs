//! HTTP surface over the memory store: the routes a dashboard, a script, or
//! another machine reaches instead of going through MCP.
//!
//! # Auth posture (stated explicitly, per this crate's own review history —
//! adding mutating routes to an unauthenticated surface is exactly the kind
//! of change that must not inherit a posture silently)
//!
//! The reference always runs authenticated: `resolve_api_key()` auto-generates
//! and persists a key on first run, so there is no unauthenticated mode short
//! of the explicit `REMIND_ME_API_KEY=disabled` opt-out. This crate does not
//! reproduce that — auto-generating a secret needs a vetted random source and
//! a place to persist it, neither of which exists here yet, and improvising
//! one for a security token is worse than not having the feature.
//!
//! Instead, [`ApiConfig::from_env`] reads [`API_KEY_ENV`]
//! (`REMIND_ME_API_KEY`) and the posture is:
//!
//! - **Unset**: `GET` routes under `/api/*` stay open — this crate's existing
//!   behaviour for `/stats` before this surface grew any further, called out
//!   here rather than left for a reader to assume. Every mutating route
//!   (`POST`/`PUT`/`PATCH`/`DELETE`) is refused with 401, because *that* is
//!   the part adding routes actually changes: a store that could previously
//!   only be read over HTTP could now be written to, and that must not
//!   default open.
//! - **Set**: every `/api/*` request, read or write, requires
//!   `Authorization: Bearer <key>`, compared in constant time via
//!   [`remind_me_core::webhook::constant_time_eq`] — reused, not
//!   reimplemented, so this is not a second bearer-auth implementation next
//!   to the webhook's to drift out of sync with.
//! - **`GET /health`** is always public, matching the reference's own
//!   rationale: a liveness probe has to work whether or not auth is
//!   configured, and it reveals no data.
//!
//! Every mutating `/api/*` request additionally requires a JSON
//! `Content-Type`, refusing anything else with 415 — the reference's
//! `JSONContentTypeMiddleware`, and for the same reason: a browser's
//! "simple" cross-origin request cannot carry that header without a CORS
//! preflight, so requiring it closes a CSRF hole independent of whether auth
//! is configured.
//!
//! **`GET /` serves the dashboard** (`#78`): the reference's own
//! `dashboard/App.jsx`, vendored verbatim — a client-side React component
//! that only ever calls `window.location.origin + "/api"`, so it needed no
//! adaptation to run against this crate's own `/api/*` routes. CORS
//! ([`http::cors_allowed_origin`]) matches the reference's `CORSMiddleware`
//! exactly (`allow_origin_regex=r"http://(localhost|127\.0\.0\.1)(:\d+)?"`,
//! every method and header allowed), confirmed from source rather than
//! assumed, and applied to every response — not just `/api/*` — the same way
//! Starlette's middleware wraps the whole app.
//!
//! # One request at a time
//!
//! Connections are accepted and served one at a time on a single thread —
//! the same shape as [`remind_me_core::webhook`], and the same reasoning:
//! every handler takes the database's lock to do anything, so concurrent
//! connections would not finish faster; they would only move the queue from
//! the kernel's accept backlog into this process.

mod auto_compile;
mod http;
mod routes;

pub use auto_compile::{AutoCompile, Compiler, RunStatus, Started};

use http::{match_pattern, Body};
use remind_me_core::daemon::settings::digest;
use remind_me_core::webhook::constant_time_eq;
use remind_me_core::wiki_fs::Wiki;
use remind_me_core::Database;
use routes::ROUTES;
use serde_json::json;
use std::io;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

/// Bearer token required for a mutating `/api/*` request; also required for
/// every `/api/*` request, mutating or not, once set. See the module docs.
pub const API_KEY_ENV: &str = "REMIND_ME_API_KEY";

const MUTATING_METHODS: [&str; 4] = ["POST", "PUT", "PATCH", "DELETE"];

/// Where the dashboard starts and watches automatic compiles.
const AUTO_COMPILE_PATH: &str = "/api/wiki/compile/auto";

/// `max_batches` from a start request: 1 when absent, 1 to
/// [`auto_compile::MAX_BATCHES`] otherwise.
fn auto_batches(request: &http::Request) -> Result<usize, String> {
    if request.body.is_empty() {
        return Ok(1);
    }
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).map_err(|e| format!("Invalid JSON body: {e}"))?;
    match body.get("max_batches") {
        None | Some(serde_json::Value::Null) => Ok(1),
        Some(value) => value
            .as_u64()
            .map(|n| n as usize)
            .filter(|n| (1..=auto_compile::MAX_BATCHES).contains(n))
            .ok_or_else(|| {
                format!(
                    "'max_batches' must be a number from 1 to {}",
                    auto_compile::MAX_BATCHES
                )
            }),
    }
}

/// Per-read and per-write deadline on a client connection.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ApiServer {
    db: Arc<Database>,
    wiki: Wiki,
    /// SHA-256 hex digest of `REMIND_ME_API_KEY`, as the store daemon's
    /// settings fingerprint carries it, so a key that arrives from a
    /// relaying unit is checked the same way as this process's own.
    api_key_digest: Option<String>,
    /// The dashboard's automatic compile runs (`/api/wiki/compile/auto`).
    auto: AutoCompile,
}

impl ApiServer {
    pub fn new(db: Database) -> Self {
        Self::with_wiki(db, Wiki::from_env())
    }

    /// Build a server against a specific wiki directory.
    ///
    /// Tests need this: the default root is a real shared directory, so a
    /// test using it would write into whatever wiki the machine's user
    /// actually has.
    pub fn with_wiki(db: Database, wiki: Wiki) -> Self {
        Self::shared(Arc::new(db), wiki)
    }

    /// Build a server over a database other servers also use: the daemon
    /// runs this and the MCP server over one store.
    pub fn shared(db: Arc<Database>, wiki: Wiki) -> Self {
        Self {
            db,
            wiki,
            api_key_digest: resolve_api_key().map(|key| digest(&key)),
            auto: AutoCompile::configured(),
        }
    }

    /// Run automatic compiles with `compiler` instead of the configured
    /// model — tests have no model to call.
    pub fn with_auto_compiler(mut self, compiler: Arc<Compiler>) -> Self {
        self.auto = AutoCompile::new(compiler);
        self
    }

    /// Override the resolved key — tests need a deterministic key rather
    /// than whatever happens to be in the process environment.
    pub fn with_api_key(mut self, key: Option<String>) -> Self {
        self.api_key_digest = key.filter(|k| !k.is_empty()).map(|k| digest(&k));
        self
    }

    pub fn status(&self) -> serde_json::Value {
        json!({
            "status": "ok",
            "server": "rusty_remind_me_api",
            "version": "0.1.0"
        })
    }

    /// Bind `addr` and serve forever, one connection at a time.
    pub fn run(&self, addr: &str) -> io::Result<()> {
        // The dashboard is a person at a browser, not a model.
        remind_me_core::context::set_default_writer(remind_me_core::context::Writer::Human);
        let listener = TcpListener::bind(addr)?;
        println!("REST API server listening on http://{}", addr);
        loop {
            let (mut stream, _peer) = listener.accept()?;
            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
            let _ = self.serve_one(&mut stream);
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Handle exactly one request on an already-accepted connection.
    ///
    /// Exposed so tests can drive the whole stack — routing, auth, the
    /// content-type check — without a real socket.
    pub fn serve_one<S: io::Read + io::Write>(&self, stream: &mut S) -> io::Result<()> {
        self.serve_one_for(stream, self.api_key_digest.as_deref())
    }

    /// [`Self::serve_one`], guarded by the API key whose digest is
    /// `api_key_digest` instead of this process's own. The store daemon
    /// serves each dashboard connection this way, under the key of the unit
    /// that relayed it (card #742): a unit with a key is never served open
    /// by a daemon without one, and a unit without one is not locked out by
    /// a daemon's key.
    pub fn serve_one_for<S: io::Read + io::Write>(
        &self,
        stream: &mut S,
        api_key_digest: Option<&str>,
    ) -> io::Result<()> {
        let Some(request) = http::read_request(stream)? else {
            return Ok(());
        };
        let cors = http::cors_allowed_origin(&request.origin);

        // A CORS preflight never carries auth and never reaches a real route
        // -- answered here, uniformly, before anything else looks at the
        // request. A non-matching origin gets a bare 200 with no CORS
        // headers, which is what makes the browser refuse to read the
        // actual response it's asking permission to send.
        if request.method == "OPTIONS" {
            return http::write_response_cors(stream, 200, Body::Json(json!({})), cors);
        }

        if request.path == "/health" {
            let (status, body) =
                routes::health(&self.db.store(), &self.wiki, &request, &Default::default());
            return http::write_response_cors(stream, status, body, cors);
        }

        // Secret-path auth, before the header gate. A calendar app polls
        // this URL from its provider's servers with no way to attach an
        // Authorization header, so the credential has to be the path itself.
        // This is the only route exempted, and it authenticates itself.
        if request.method == "GET" {
            if let Some(token) = request
                .path
                .strip_prefix("/api/reminders/")
                .and_then(|rest| rest.strip_suffix(".ics"))
            {
                let (status, body) = routes::api_reminders_ics(&self.db.store(), token);
                return http::write_response_cors(stream, status, body, cors);
            }
        }

        if request.path.starts_with("/api/") {
            if let Some(response) = check_auth(&request, api_key_digest) {
                return http::write_response_cors(stream, response.0, response.1, cors);
            }
            if MUTATING_METHODS.contains(&request.method.as_str()) {
                let content_type = request.content_type.split(';').next().unwrap_or("").trim();
                if content_type != "application/json" {
                    return http::write_response_cors(
                        stream,
                        415,
                        Body::Json(json!({ "error": "Content-Type must be application/json" })),
                        cors,
                    );
                }
            }
        }

        let (status, body) = if request.path == AUTO_COMPILE_PATH {
            self.auto_compile(&request)
        } else {
            self.dispatch(&request)
        };
        http::write_response_cors(stream, status, body, cors)
    }

    /// `GET` reports the current or last automatic compile; `POST` starts
    /// one (`{"max_batches": 1..=20}`, default 1) and answers 202 at once,
    /// or 409 while one runs. Outside [`ROUTES`] because it needs the
    /// database itself, not a store handle: see [`auto_compile`].
    fn auto_compile(&self, request: &http::Request) -> (u16, Body) {
        match request.method.as_str() {
            "GET" => (200, Body::Json(json!(self.auto.status()))),
            "POST" => {
                let max_batches = match auto_batches(request) {
                    Ok(n) => n,
                    Err(why) => return (400, Body::Json(json!({ "error": why }))),
                };
                let options = remind_me_core::wiki_compile::AutoOptions {
                    max_batches,
                    ..Default::default()
                };
                match self
                    .auto
                    .start(Arc::clone(&self.db), self.wiki.clone(), options)
                {
                    Started::Now(status) => (202, Body::Json(json!(status))),
                    Started::Busy(status) => (
                        409,
                        Body::Json(json!({
                            "error": "a compile is already running",
                            "status": status,
                        })),
                    ),
                }
            }
            _ => (405, Body::Json(json!({ "error": "Method Not Allowed" }))),
        }
    }

    fn dispatch(&self, request: &http::Request) -> (u16, Body) {
        let mut path_matched = false;
        for route in ROUTES {
            let Some(params) = match_pattern(route.pattern, &request.path) else {
                continue;
            };
            path_matched = true;
            if route.methods.contains(&request.method.as_str()) {
                let store = self.db.store();
                return (route.handler)(&store, &self.wiki, request, &params);
            }
        }
        if path_matched {
            (405, Body::Json(json!({ "error": "Method Not Allowed" })))
        } else {
            (404, Body::Json(json!({ "error": "Not Found" })))
        }
    }
}

/// `None` when the request may proceed; `Some((status, body))` with the
/// response to send otherwise.
fn check_auth(request: &http::Request, api_key_digest: Option<&str>) -> Option<(u16, Body)> {
    match api_key_digest {
        Some(expected) => {
            let presented = request.authorization.strip_prefix("Bearer ").map(digest);
            if presented.is_some_and(|got| constant_time_eq(got.as_bytes(), expected.as_bytes())) {
                // The flat key is always read-write, exactly as it was
                // before scopes existed. Adding scopes must not
                // retroactively restrict a deployment already relying on
                // it.
                return None;
            }

            // A named, scope-limited key (issue #120). Checked only after
            // the flat key misses, so the common path is one comparison.
            if let Some(presented) =
                rusty_oauth::bearer::token_from_authorization(&request.authorization)
            {
                if let Some(verified) = remind_me_core::api_keys::verify(presented) {
                    if verified.may_use(&request.method) {
                        return None;
                    }
                    // 403, not 401: the key is genuine and was
                    // authenticated. Answering 401 would send the holder
                    // looking for a credentials problem they do not have.
                    return Some((
                        403,
                        Body::Json(json!({
                            "error": format!(
                                "API key '{}' is read-only (scope=read); {} requires a read-write key",
                                verified.name, request.method
                            )
                        })),
                    ));
                }
            }

            Some((401, Body::Json(json!({ "error": "Unauthorized" }))))
        }
        None => {
            if MUTATING_METHODS.contains(&request.method.as_str()) {
                Some((
                    401,
                    Body::Json(json!({
                        "error": "unauthenticated write access is disabled; set REMIND_ME_API_KEY to enable it"
                    })),
                ))
            } else {
                None
            }
        }
    }
}

fn resolve_api_key() -> Option<String> {
    std::env::var(API_KEY_ENV).ok().filter(|k| !k.is_empty())
}
