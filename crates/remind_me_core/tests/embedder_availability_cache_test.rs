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
    available_embedder, EMBEDDING_BACKEND_ENV, EMBEDDING_DIM_ENV, OLLAMA_URL_ENV,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;

/// Held by every test here that touches the embedding environment, matching
/// the convention in `status_test.rs` and `remind_me_mcp`'s own suite.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// A stub that answers the `embed` probe well enough to count as reachable,
/// on a port the OS picks so two runs never collide.
fn fake_daemon() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let body = r#"{"embeddings":[[0.1,0.2]]}"#;
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        );
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

    // Bind and immediately drop, so the port is one nothing is listening on.
    let dead_port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    set_backend(dead_port);
    assert!(
        available_embedder().is_none(),
        "an unreachable daemon is unavailable"
    );

    // Same backend, same model, same dimension -- only the address differs,
    // which is exactly the case an identity-based key would have missed.
    let (live_port, handle) = fake_daemon();
    set_backend(live_port);
    let available = available_embedder().is_some();

    clear_backend();
    let _ = handle.join();

    assert!(
        available,
        "a reachable daemon must be reported available immediately, not after \
         the previous address's failure TTL expires"
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
    let _ = handle.join();

    let second = available_embedder().is_some();
    clear_backend();

    assert!(
        second,
        "the second call must be served from the cache -- the stub answered \
         only one connection, so a re-probe would have found nothing"
    );
}
