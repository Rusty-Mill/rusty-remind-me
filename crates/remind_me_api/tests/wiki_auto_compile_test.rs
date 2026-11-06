//! `POST`/`GET /api/wiki/compile/auto`: the dashboard starting the automatic
//! wiki compile and watching it, with a fake compiler standing in for the
//! model.

mod common;
use common::{authed_get, authed_json, authed_server, unauthed_json, Response};
use remind_me_api::{ApiServer, Compiler};
use remind_me_core::wiki_compile::AutoReport;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PATH: &str = "/api/wiki/compile/auto";

/// A compiler that reports the batches it was asked for, once `release`
/// sends it a go; it records each run's `max_batches`.
fn gated(
    result: Result<AutoReport, String>,
) -> (Arc<Compiler>, Sender<()>, Arc<Mutex<Vec<usize>>>) {
    let (release, gate): (Sender<()>, Receiver<()>) = channel();
    let gate = Mutex::new(gate);
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    let compiler: Arc<Compiler> = Arc::new(move |_wiki, _db, options| {
        seen.lock().unwrap().push(options.max_batches);
        gate.lock().unwrap().recv().unwrap();
        result.clone()
    });
    (compiler, release, asked)
}

/// The status once the run has finished, polled for up to five seconds.
fn finished(server: &ApiServer) -> Response {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = authed_get(server, PATH);
        if status.json()["running"] == false || Instant::now() > deadline {
            return status;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_run_starts_at_once_refuses_a_second_and_reports_when_done() {
    let report = AutoReport {
        batches: 2,
        sources_integrated: 37,
        pages_written: vec!["Datastar".into()],
        pending: 5,
        stopped: None,
    };
    let (compiler, release, asked) = gated(Ok(report));
    let (server, root) = authed_server("auto-compile-run");
    let server = server.with_auto_compiler(compiler);

    let idle = authed_get(&server, PATH);
    assert_eq!(idle.status, 200);
    assert_eq!(idle.json()["running"], false);
    assert_eq!(idle.json()["max_batches"], 0);

    let started = authed_json(&server, "POST", PATH, r#"{"max_batches": 2}"#);
    assert_eq!(started.status, 202);
    assert_eq!(started.json()["running"], true);
    assert!(started.json()["started_ms"].as_u64().unwrap() > 0);

    let second = authed_json(&server, "POST", PATH, "{}");
    assert_eq!(second.status, 409);
    assert!(second.json()["error"]
        .as_str()
        .unwrap()
        .contains("already running"));

    // Other routes still answer while the run waits on its "model".
    assert_eq!(authed_get(&server, "/api/wiki/status").status, 200);

    release.send(()).unwrap();
    let done = finished(&server).json();
    assert_eq!(done["running"], false);
    assert_eq!(done["report"]["sources_integrated"], 37);
    assert_eq!(done["report"]["pages_written"][0], "Datastar");
    assert!(done["finished_ms"].as_u64().unwrap() >= done["started_ms"].as_u64().unwrap());
    assert_eq!(*asked.lock().unwrap(), [2], "the second start never ran");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_run_that_cannot_start_reports_why_and_can_be_retried() {
    let (compiler, release, asked) = gated(Err("automatic compile needs a local model".into()));
    let (server, root) = authed_server("auto-compile-error");
    let server = server.with_auto_compiler(compiler);

    assert_eq!(authed_json(&server, "POST", PATH, "").status, 202);
    release.send(()).unwrap();
    let done = finished(&server).json();
    assert!(done["error"].as_str().unwrap().contains("local model"));
    assert!(done.get("report").is_none());

    // A finished run frees the slot.
    assert_eq!(authed_json(&server, "POST", PATH, "").status, 202);
    release.send(()).unwrap();
    finished(&server);
    assert_eq!(
        *asked.lock().unwrap(),
        [1, 1],
        "an empty body asks for one batch"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_panicking_run_does_not_stay_running() {
    let compiler: Arc<Compiler> = Arc::new(|_, _, _| panic!("model adapter bug"));
    let (server, root) = authed_server("auto-compile-panic");
    let server = server.with_auto_compiler(compiler);

    assert_eq!(authed_json(&server, "POST", PATH, "{}").status, 202);
    let done = finished(&server).json();
    assert_eq!(done["running"], false);
    assert!(done["error"].as_str().unwrap().contains("panicked"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn max_batches_must_be_between_1_and_20() {
    let (compiler, _release, asked) = gated(Ok(AutoReport::default()));
    let (server, root) = authed_server("auto-compile-bounds");
    let server = server.with_auto_compiler(compiler);

    for body in [
        r#"{"max_batches": 0}"#,
        r#"{"max_batches": 21}"#,
        r#"{"max_batches": "2"}"#,
        "{",
    ] {
        let response = authed_json(&server, "POST", PATH, body);
        assert_eq!(response.status, 400, "{body}");
    }
    assert!(asked.lock().unwrap().is_empty());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn starting_a_run_needs_the_api_key_and_other_methods_are_refused() {
    let (compiler, _release, asked) = gated(Ok(AutoReport::default()));
    let (server, root) = authed_server("auto-compile-auth");
    let server = server.with_auto_compiler(compiler);

    assert_eq!(unauthed_json(&server, "POST", PATH, "{}").status, 401);
    assert_eq!(authed_json(&server, "DELETE", PATH, "").status, 405);
    assert!(asked.lock().unwrap().is_empty());
    std::fs::remove_dir_all(&root).unwrap();
}
