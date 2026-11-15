//! `available_embedder`'s availability cache must expire on a configuration
//! change, not only on its TTL.
//!
//! The cache exists so the hot search path does not pay a ping round-trip per
//! call, and a failed probe is held for 30 seconds. Keyed on time alone, that
//! verdict outlived the thing it was a verdict *about*: point the backend at a
//! daemon that is actually up, and the answer stayed "not answering" for the
//! rest of the TTL, because nothing invalidated the failure the old address
//! earned. `resolve_embedder`'s own documentation promises the opposite — "a
//! configuration change takes effect on the next search rather than requiring
//! a restart" — and the availability-gated resolver quietly did not honour it.
//!
//! This surfaced as an intermittent CI failure rather than a bug report:
//! `remind_me_mcp`'s `test_server_status_reports_embeddings_active_...` binds
//! a fake daemon on a fresh port, and inherited the stale failure from
//! whichever test had last probed an unreachable one in the same process. The
//! `ENV_LOCK` convention those tests follow serialises the environment, but a
//! process-global cache is not the environment, so the lock never covered it.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::embedder::{
    available_embedder, EMBEDDING_BACKEND_ENV, EMBEDDING_DIM_ENV, OLLAMA_MODEL_ENV, OLLAMA_URL_ENV,
    ONNX_MODEL_PATH_ENV, ONNX_TOKENIZER_PATH_ENV, OPENAI_EMBED_MODEL_ENV, OPENAI_EMBED_URL_ENV,
};
use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Held by every test here that touches the embedding environment, matching
/// the convention in `status_test.rs` and `remind_me_mcp`'s own suite.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// How long the stub waits for its one probe, and for that probe's bytes.
/// Bounded so a test whose probe never arrives (a cache hit it did not
/// expect) fails on its assertion instead of hanging on `join`.
const STUB_PATIENCE: Duration = Duration::from_secs(10);

/// A stub that answers the `embed` probe well enough to count as reachable,
/// on a port the OS picks so two runs never collide. It serves at most one
/// connection, gives up after [`STUB_PATIENCE`], and its thread returns
/// whether it served one.
fn fake_daemon() -> (u16, std::thread::JoinHandle<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + STUB_PATIENCE;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return false,
            }
        };
        let bounded = stream.set_nonblocking(false).is_ok()
            && stream.set_read_timeout(Some(STUB_PATIENCE)).is_ok()
            && stream.set_write_timeout(Some(STUB_PATIENCE)).is_ok();
        if !bounded {
            return false;
        }
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let body = r#"{"embeddings":[[0.1,0.2]]}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .is_ok()
    });
    (port, handle)
}

fn set_backend(port: u16) {
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    crate::test_env::set_var(OLLAMA_URL_ENV, format!("http://127.0.0.1:{port}"));
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "2");
}

fn clear_backend() {
    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    crate::test_env::remove_var(OLLAMA_URL_ENV);
    crate::test_env::remove_var(EMBEDDING_DIM_ENV);
}

/// A dead address, then a live one, with no wait in between.
///
/// This is the regression: before the cache was keyed on the configuration,
/// the second call returned the first call's cached failure, because under 30
/// seconds had passed. It asserts the ordering that actually bit — failure
/// first — since a stale *success* would be caught by any test that expects
/// an unconfigured backend to report unavailable.
#[test]
fn a_failed_probe_does_not_answer_for_a_different_backend_address() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // The live stub binds first, so the dead port below cannot be its port:
    // a released port the stub then reused would hit the cached failure, the
    // stub would never be probed, and the test would hang on its thread.
    let (live_port, handle) = fake_daemon();
    // Bind and immediately drop, so the port is one nothing is listening on.
    let dead_port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    assert_ne!(dead_port, live_port, "the live stub still holds its port");
    set_backend(dead_port);
    assert!(
        available_embedder().is_none(),
        "an unreachable daemon is unavailable"
    );

    // Same backend, same model, same dimension -- only the address differs,
    // which is exactly the case an identity-based key would have missed.
    set_backend(live_port);
    let available = available_embedder().is_some();

    clear_backend();
    let served = handle.join().expect("stub thread panicked");

    assert!(
        available,
        "a reachable daemon must be reported available immediately, not after \
         the previous address's failure TTL expires"
    );
    assert!(
        served,
        "the live address must be probed, not answered from a cache"
    );
}

/// The cache still does its job within one configuration: a second call does
/// not re-probe, which is the whole reason it exists.
///
/// Asserted by taking the stub away. The daemon answers exactly one
/// connection, so if the second call re-probed it would find nothing
/// listening and report unavailable.
#[test]
fn a_successful_probe_is_still_cached_for_the_same_configuration() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let (port, handle) = fake_daemon();
    set_backend(port);

    assert!(
        available_embedder().is_some(),
        "the stub answers the first probe"
    );
    assert!(
        handle.join().expect("stub thread panicked"),
        "the first call must reach the stub, not a cached verdict"
    );

    let second = available_embedder().is_some();
    clear_backend();

    assert!(
        second,
        "the second call must be served from the cache -- the stub answered \
         only one connection, so a re-probe would have found nothing"
    );
}

/// A stub that answers every connection with one OpenAI-style embedding and
/// counts them, until `stop` is set. Ollama probes reach it too and fail to
/// parse the reply, which still counts as a probe.
fn counting_stub() -> (u16, Arc<AtomicUsize>, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let (probes, stop) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let (count, done) = (Arc::clone(&probes), Arc::clone(&stop));
    std::thread::spawn(move || {
        let deadline = Instant::now() + STUB_PATIENCE;
        while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            count.fetch_add(1, Ordering::SeqCst);
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(STUB_PATIENCE));
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = r#"{"data":[{"index":0,"embedding":[0.6,0.8]}]}"#;
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            );
        }
    });
    (port, probes, stop)
}

/// Every variable the cache is keyed on invalidates it: changing any one of
/// them makes the next call probe again (#352).
///
/// This list mirrors `config_fingerprint`'s. A variable the resolver reads but
/// the fingerprint omits is one you can change without the cache noticing.
#[test]
fn changing_any_fingerprint_variable_invalidates_the_cache() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (port, probes, stop) = counting_stub();
    let url = format!("http://127.0.0.1:{port}");
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "openai");
    crate::test_env::set_var(EMBEDDING_DIM_ENV, "2");
    crate::test_env::set_var(OPENAI_EMBED_URL_ENV, &url);
    crate::test_env::set_var(OPENAI_EMBED_MODEL_ENV, "m");
    crate::test_env::set_var(OLLAMA_URL_ENV, &url);

    let first = available_embedder().is_some();
    let second = available_embedder().is_some();
    let cached = probes.load(Ordering::SeqCst);

    // Each change keeps a probe reaching the stub, so a re-probe is counted
    // whether it succeeds or not.
    let changes = [
        (OPENAI_EMBED_MODEL_ENV, "m2".to_string()),
        (OPENAI_EMBED_URL_ENV, format!("{url}/v1")),
        (OLLAMA_MODEL_ENV, "other".to_string()),
        (ONNX_MODEL_PATH_ENV, "/no/model.rten".to_string()),
        (ONNX_TOKENIZER_PATH_ENV, "/no/tokenizer.json".to_string()),
        (EMBEDDING_DIM_ENV, "3".to_string()),
        (EMBEDDING_BACKEND_ENV, "ollama".to_string()),
        (OLLAMA_URL_ENV, format!("{url}/")),
    ];
    let mut missed = Vec::new();
    for (key, value) in &changes {
        let before = probes.load(Ordering::SeqCst);
        crate::test_env::set_var(key, value);
        let _ = available_embedder();
        if probes.load(Ordering::SeqCst) == before {
            missed.push(*key);
        }
    }

    stop.store(true, Ordering::Relaxed);
    for (key, _) in &changes {
        crate::test_env::remove_var(key);
    }
    assert!(first && second, "the stub answers the openai probe");
    assert_eq!(
        cached, 1,
        "an unchanged configuration is served from the cache"
    );
    assert!(missed.is_empty(), "a change to {missed:?} did not re-probe");
}
