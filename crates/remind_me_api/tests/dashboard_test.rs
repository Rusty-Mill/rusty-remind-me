//! Coverage for `GET /` (the vendored dashboard) and the CORS policy
//! (`#78`) — confirmed against the reference's own `CORSMiddleware` setup
//! (`allow_origin_regex=r"http://(localhost|127\.0\.0\.1)(:\d+)?"`,
//! `allow_methods=["*"]`, `allow_headers=["*"]`) rather than assumed.

mod common;
use common::{authed_json, authed_server, call, call_full, call_with_origin, get, server, KEY};

#[test]
fn the_dashboard_route_serves_html_embedding_the_vendored_jsx() {
    let (server, root) = server("dashboard");
    let response = get(&server, "/");
    assert_eq!(response.status, 200);
    assert!(response.content_type.starts_with("text/html"));
    assert!(response.body.contains("<div id=\"root\"></div>"));
    assert!(
        response.body.contains("text/babel"),
        "the JSX must be embedded in a Babel script block"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

/// The dashboard's source files are all served, in dependency order.
///
/// There is no bundler here: each file is its own `<script type="text/babel">`
/// block, and those share one global scope and run in document order. A file
/// may reference anything declared above it and nothing below it, so the order
/// of `DASHBOARD_SOURCES` is load-bearing in a way nothing else checks —
/// reordering it would leave every test green and the page blank, because the
/// failure is a `ReferenceError` in a browser this suite never opens.
#[test]
fn the_dashboard_sources_are_served_in_dependency_order() {
    let (server, root) = server("dashboard-source-order");
    let page = get(&server, "/").body;

    // Theme first (everything styles against it), shell last (it references
    // every hook, component and form declared before it).
    let expected = [
        "theme.jsx",
        "api.jsx",
        "stores.jsx",
        "icons.jsx",
        "components.jsx",
        "forms.jsx",
        "app.jsx",
    ];

    let mut previous = 0;
    for name in expected {
        let marker = format!("data-file=\"{}\"", name);
        let at = page.find(&marker).unwrap_or_else(|| {
            panic!("the dashboard no longer serves {name}");
        });
        assert!(
            at > previous,
            "{name} is served out of dependency order — a file can only \
             reference what loaded before it"
        );
        previous = at;
    }

    // The render call is what turns the definitions above into a page, so it
    // has to be in the last block rather than merely present somewhere.
    let render_at = page
        .find("ReactDOM.createRoot")
        .expect("the dashboard still mounts itself");
    let shell_at = page.find("data-file=\"app.jsx\"").expect("the shell block");
    assert!(
        render_at > shell_at,
        "the mount call must live in the last block, after every definition"
    );

    std::fs::remove_dir_all(&root).unwrap();
}

/// Every command endpoint the dashboard drives is both named in the page it
/// serves and answered by the route table behind it.
///
/// The JSX is a string constant to the Rust build: nothing type-checks that a
/// path the page fetches is a path this server routes, so a renamed route
/// would break the dashboard silently and no Rust test would notice. This
/// walks both halves — the literal appears in the served page, and a real
/// request to it is not a 404 — which is the cheapest thing that actually
/// fails when the two drift apart.
#[test]
fn every_command_endpoint_the_dashboard_calls_is_a_real_route() {
    let (server, root) = server("dashboard-endpoints");
    let page = get(&server, "/").body;

    for path in [
        "/reminders",
        "/saved-searches",
        "/digest",
        "/status",
        "/memories",
        "/stats",
        "/vitality",
        "/wiki",
        "/wiki/schema",
    ] {
        assert!(
            page.contains(&format!("\"{}", path)),
            "the dashboard no longer references {:?}",
            path
        );
        let response = get(&server, &format!("/api{}", path));
        assert_ne!(
            response.status, 404,
            "the dashboard calls /api{} but nothing routes it",
            path
        );
    }

    // The one path built by concatenation rather than written whole, so the
    // literal check above cannot see it.
    assert!(
        page.contains("\"/saved-searches/\""),
        "the dashboard no longer builds the run-a-saved-search path"
    );
    assert_eq!(
        get(&server, "/api/saved-searches/nothing-saved/run").status,
        404,
        "404 for the absent search, not for an absent route"
    );

    std::fs::remove_dir_all(&root).unwrap();
}

/// As above, for the endpoints the dashboard only ever reaches with a write.
///
/// A `GET` cannot stand in for these: an unrouted `/api/*` path answers 401 on
/// a keyed server and falls through to the page lookup on an unkeyed one, so
/// neither status would distinguish "routed" from "not routed". Each is
/// therefore driven with the method it actually uses, against an authenticated
/// server, and asserted on a response only its own handler produces.
#[test]
fn every_wiki_write_endpoint_the_dashboard_calls_is_a_real_route() {
    let (server, root) = authed_server("dashboard-write-endpoints");
    let page = get(&server, "/").body;

    for literal in ["\"/wiki\"", "\"/wiki/compile\"", "\"/wiki/\""] {
        assert!(
            page.contains(literal),
            "the dashboard no longer references {}",
            literal
        );
    }

    // POST /api/wiki reaches the write handler: only that handler answers 400
    // naming the missing field. An absent route would be 405.
    let write = authed_json(&server, "POST", "/api/wiki", "{}");
    assert_eq!(write.status, 400);
    assert!(write.json()["error"].as_str().unwrap().contains("title"));

    // POST /api/wiki/compile reaches the compile handler, which reports a
    // tagged status no other route produces.
    let compile = authed_json(&server, "POST", "/api/wiki/compile", "{}");
    assert_eq!(compile.status, 200);
    assert!(compile.json()["status"].is_string());

    // DELETE /api/wiki/{slug} reaches the delete handler: 404 for the absent
    // page, which is the handler's own answer rather than the router's.
    let delete = call(
        &server,
        "DELETE",
        "/api/wiki/nothing-written-here",
        Some(&format!("Bearer {}", KEY)),
        Some("application/json"),
        "",
    );
    assert_eq!(delete.status, 404);
    assert!(delete.json()["error"]
        .as_str()
        .unwrap()
        .contains("Wiki page not found"));

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_dashboard_is_unauthenticated_even_when_an_api_key_is_configured() {
    // The reference's own Route("/", index) isn't under
    // protect_prefix="/api/" -- serving the dashboard page itself never
    // requires the API key the /api/* routes need.
    let (server, root) = server("dashboard-authed");
    let server = server.with_api_key(Some("s3cret".to_string()));
    let response = get(&server, "/");
    assert_eq!(response.status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_localhost_origin_gets_reflected_cors_headers() {
    let (server, root) = server("cors-localhost");
    let response = call_with_origin(&server, "GET", "/api/stats", "http://localhost:5173");
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("http://localhost:5173")
    );
    assert_eq!(response.header("access-control-allow-methods"), Some("*"));
    assert_eq!(response.header("access-control-allow-headers"), Some("*"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_127_0_0_1_origin_with_no_port_is_also_allowed() {
    let (server, root) = server("cors-loopback");
    let response = call_with_origin(&server, "GET", "/health", "http://127.0.0.1");
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("http://127.0.0.1")
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_foreign_origin_gets_no_cors_headers_at_all() {
    let (server, root) = server("cors-foreign");
    let response = call_with_origin(&server, "GET", "/health", "http://evil.example");
    assert!(response.header("access-control-allow-origin").is_none());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_https_origin_is_rejected_even_on_localhost() {
    // The reference's regex is anchored to the http:// scheme literally.
    let (server, root) = server("cors-https");
    let response = call_with_origin(&server, "GET", "/health", "https://localhost:5173");
    assert!(response.header("access-control-allow-origin").is_none());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_request_with_no_origin_header_gets_no_cors_headers() {
    // The ordinary script/curl case -- CORS is a browser-enforced policy,
    // and nothing here needs it to work non-interactively.
    let (server, root) = server("cors-no-origin");
    let response = get(&server, "/health");
    assert!(response.header("access-control-allow-origin").is_none());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_options_preflight_from_an_allowed_origin_gets_cors_headers_with_no_route_dispatch() {
    let (server, root) = server("cors-preflight");
    let response = call_with_origin(&server, "OPTIONS", "/api/memories", "http://localhost:3000");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("http://localhost:3000")
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_options_preflight_from_a_foreign_origin_gets_no_cors_headers() {
    let (server, root) = server("cors-preflight-foreign");
    let response = call_with_origin(&server, "OPTIONS", "/api/memories", "http://evil.example");
    assert!(response.header("access-control-allow-origin").is_none());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn cors_headers_are_added_to_error_responses_too() {
    // The reference's CORSMiddleware wraps the whole app, including a 404 --
    // a browser reading a failed request's error still needs the header.
    let (server, root) = server("cors-on-404");
    let response = call_with_origin(&server, "GET", "/not-a-real-path", "http://localhost:1234");
    assert_eq!(response.status, 404);
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("http://localhost:1234")
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn cors_headers_are_added_to_a_401_from_a_missing_api_key_too() {
    let (server, root) = server("cors-on-401");
    let server = server.with_api_key(Some("s3cret".to_string()));
    let response = call_full(
        &server,
        "POST",
        "/api/memories",
        Some("http://localhost:1234"),
        None,
        Some("application/json"),
        "{}",
    );
    assert_eq!(response.status, 401);
    assert_eq!(
        response.header("access-control-allow-origin"),
        Some("http://localhost:1234")
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_dashboard_route_still_works_when_called_through_the_normal_call_helper() {
    let (server, root) = server("dashboard-via-call");
    let response = call(&server, "GET", "/", None, None, "");
    assert_eq!(response.status, 200);
    std::fs::remove_dir_all(&root).unwrap();
}
