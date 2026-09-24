//! Coverage for HyDE query expansion (`remind_me_core::query_expansion`)
//! against a fake Ollama server — the same testing pattern
//! `ollama_embedder_test.rs` uses for `OllamaEmbedder`'s hand-rolled HTTP
//! client, since this module hand-rolls its own `/api/generate` client for
//! the reasons documented in its module doc.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::embedder::OLLAMA_URL_ENV;
use remind_me_core::query_expansion::{self, EXPANSION_MODE_ENV, HYDE_MODEL_ENV, HYDE_TIMEOUT_ENV};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

/// The env vars this module reads are process-global; every test here holds
/// `ENV_LOCK` for its duration, the same convention `reranker_test.rs`
/// establishes for its own env-var-driven module.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn clear_env() {
    for var in [
        EXPANSION_MODE_ENV,
        HYDE_MODEL_ENV,
        HYDE_TIMEOUT_ENV,
        OLLAMA_URL_ENV,
    ] {
        crate::test_env::remove_var(var);
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Reads one HTTP/1.1 request off `stream` and returns its body.
fn read_request_body(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let headers_end = loop {
        let n = stream.read(&mut chunk).expect("read request");
        assert!(n > 0, "connection closed before headers completed");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..headers_end]).to_string();
    let content_length: usize = head
        .lines()
        .find_map(|l| {
            l.to_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().to_string())
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buf.len() - headers_end < content_length {
        let n = stream.read(&mut chunk).expect("read body");
        assert!(n > 0, "connection closed before body completed");
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf[headers_end..headers_end + content_length]).to_string()
}

/// A fake Ollama server that answers exactly `responses.len()` `/api/generate`
/// requests in order (always `200 OK`), then stops accepting. Returns the
/// `http://127.0.0.1:PORT` URL to set `REMIND_ME_OLLAMA_URL` to, plus a
/// handle yielding every request body it saw.
fn fake_server(responses: Vec<String>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for response_body in responses {
            let (mut stream, _) = listener.accept().expect("accept");
            seen.push(read_request_body(&mut stream));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
        seen
    });
    (format!("http://127.0.0.1:{}", port), handle)
}

fn fake_server_with_status(status: &str, body: &str) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let status = status.to_string();
    let body = body.to_string();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        read_request_body(&mut stream);
        let response = format!(
            "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });
    (format!("http://127.0.0.1:{}", port), handle)
}

fn generate_response(text: &str) -> String {
    serde_json::json!({ "response": text }).to_string()
}

#[test]
fn hyde_passage_returns_the_generated_text_trimmed() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let (url, handle) = fake_server(vec![generate_response("  a plausible passage  ")]);
    crate::test_env::set_var(OLLAMA_URL_ENV, url);

    let passage = query_expansion::hyde_passage("what does the user prefer for editors");

    assert_eq!(passage.as_deref(), Some("a plausible passage"));
    handle.join().unwrap();
    clear_env();
}

#[test]
fn hyde_passage_sends_the_configured_model_and_the_query_in_the_prompt() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let (url, handle) = fake_server(vec![generate_response("passage")]);
    crate::test_env::set_var(OLLAMA_URL_ENV, url);
    crate::test_env::set_var(HYDE_MODEL_ENV, "custom-instruct-model");

    query_expansion::hyde_passage("quokka habitat facts");

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
    assert_eq!(body["model"], "custom-instruct-model");
    assert_eq!(body["stream"], false);
    assert_eq!(body["options"]["temperature"], 0.0);
    assert!(
        body["prompt"]
            .as_str()
            .unwrap()
            .contains("quokka habitat facts"),
        "prompt should embed the query: {:?}",
        body["prompt"]
    );
    clear_env();
}

#[test]
fn hyde_passage_truncates_to_the_max_char_cap() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let long_text = "x".repeat(900);
    let (url, handle) = fake_server(vec![generate_response(&long_text)]);
    crate::test_env::set_var(OLLAMA_URL_ENV, url);

    let passage = query_expansion::hyde_passage("query").unwrap();

    assert_eq!(passage.chars().count(), 600);
    handle.join().unwrap();
    clear_env();
}

#[test]
fn hyde_passage_is_none_for_an_empty_response() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let (url, handle) = fake_server(vec![generate_response("   ")]);
    crate::test_env::set_var(OLLAMA_URL_ENV, url);

    assert!(query_expansion::hyde_passage("query").is_none());
    handle.join().unwrap();
    clear_env();
}

#[test]
fn hyde_passage_is_none_on_a_non_2xx_status() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let (url, handle) = fake_server_with_status("404 Not Found", "model not found");
    crate::test_env::set_var(OLLAMA_URL_ENV, url);

    assert!(query_expansion::hyde_passage("query").is_none());
    handle.join().unwrap();
    clear_env();
}

#[test]
fn hyde_passage_is_none_on_an_unreachable_daemon_not_a_panic() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    // Port 1 is a reserved low port nothing binds to; a fast local refusal
    // proves this degrades rather than hangs the caller.
    crate::test_env::set_var(OLLAMA_URL_ENV, "http://127.0.0.1:1");

    assert!(query_expansion::hyde_passage("query").is_none());
    clear_env();
}

#[test]
fn expand_query_is_empty_when_disabled() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    assert!(query_expansion::expand_query("anything").is_empty());
    clear_env();
}

#[test]
fn expand_query_is_empty_for_a_blank_query_even_when_enabled() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    crate::test_env::set_var(EXPANSION_MODE_ENV, "hyde");
    assert!(query_expansion::expand_query("   ").is_empty());
    clear_env();
}

#[test]
fn expand_query_returns_the_hyde_passage_when_enabled_and_generation_succeeds() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    let (url, handle) = fake_server(vec![generate_response("a hypothetical answer")]);
    crate::test_env::set_var(OLLAMA_URL_ENV, url);
    crate::test_env::set_var(EXPANSION_MODE_ENV, "hyde");

    let expanded =
        query_expansion::expand_query("expand_query_returns_the_hyde_passage unique query");

    assert_eq!(expanded, vec!["a hypothetical answer".to_string()]);
    handle.join().unwrap();
    clear_env();
}

#[test]
fn expand_query_caches_a_successful_expansion_so_a_repeat_query_makes_no_second_request() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    // Only one response queued: if the cache did not work, the second
    // `expand_query` call would block on a connection nothing accepts and
    // eventually time out instead of returning immediately.
    let (url, handle) = fake_server(vec![generate_response("cached passage")]);
    crate::test_env::set_var(OLLAMA_URL_ENV, url);
    crate::test_env::set_var(EXPANSION_MODE_ENV, "hyde");
    crate::test_env::set_var(HYDE_TIMEOUT_ENV, "2");
    let query = "expand_query_caches_a_successful_expansion unique query";

    let first = query_expansion::expand_query(query);
    let second = query_expansion::expand_query(query);

    assert_eq!(first, vec!["cached passage".to_string()]);
    assert_eq!(
        second, first,
        "the cached call should return the same expansion"
    );
    let requests = handle.join().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "the cache hit must not touch the network"
    );
    clear_env();
}

#[test]
fn expand_query_is_disabled_for_a_mode_other_than_hyde() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_env();
    crate::test_env::set_var(EXPANSION_MODE_ENV, "off");
    assert!(query_expansion::expand_query("query").is_empty());
    clear_env();
}
