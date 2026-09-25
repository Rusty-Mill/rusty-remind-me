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

mod http;
mod routes;

use http::{match_pattern, Body};
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

/// Per-read and per-write deadline on a client connection.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ApiServer {
    db: Arc<Database>,
    wiki: Wiki,
    api_key: Option<String>,
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
        Self {
            db: Arc::new(db),
            wiki,
            api_key: resolve_api_key(),
        }
    }

    /// Override the resolved key — tests need a deterministic key rather
    /// than whatever happens to be in the process environment.
    pub fn with_api_key(mut self, key: Option<String>) -> Self {
        self.api_key = key.filter(|k| !k.is_empty());
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
                routes::health(&self.db.conn(), &self.wiki, &request, &Default::default());
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
                let (status, body) = routes::api_reminders_ics(&self.db.conn(), token);
                return http::write_response_cors(stream, status, body, cors);
            }
        }

        if request.path.starts_with("/api/") {
            if let Some(response) = self.check_auth(&request) {
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

        let (status, body) = self.dispatch(&request);
        http::write_response_cors(stream, status, body, cors)
    }

    /// `None` when the request may proceed; `Some((status, body))` with the
    /// response to send otherwise.
    fn check_auth(&self, request: &http::Request) -> Option<(u16, Body)> {
        match &self.api_key {
            Some(key) => {
                let expected = format!("Bearer {}", key);
                if constant_time_eq(request.authorization.as_bytes(), expected.as_bytes()) {
                    // The flat key is always read-write, exactly as it was
                    // before scopes existed. Adding scopes must not
                    // retroactively restrict a deployment already relying on
                    // it.
                    return None;
                }

                // A named, scope-limited key (issue #120). Checked only after
                // the flat key misses, so the common path is one comparison.
                if let Some(presented) = request.authorization.strip_prefix("Bearer ") {
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

    fn dispatch(&self, request: &http::Request) -> (u16, Body) {
        let mut path_matched = false;
        for route in ROUTES {
            let Some(params) = match_pattern(route.pattern, &request.path) else {
                continue;
            };
            path_matched = true;
            if route.methods.contains(&request.method.as_str()) {
                let conn = self.db.conn();
                return (route.handler)(&conn, &self.wiki, request, &params);
            }
        }
        if path_matched {
            (405, Body::Json(json!({ "error": "Method Not Allowed" })))
        } else {
            (404, Body::Json(json!({ "error": "Not Found" })))
        }
    }
}

fn resolve_api_key() -> Option<String> {
    std::env::var(API_KEY_ENV).ok().filter(|k| !k.is_empty())
}
