//! Coverage for `OllamaEmbedder`'s hand-rolled HTTP client against a fake
//! Ollama server (a bare `TcpListener`, matching the same testing pattern
//! `webhook_test.rs` uses for the inbound side of this crate's other
//! hand-rolled HTTP code).

use remind_me_core::embedder::{
    EmbedRole, Embedder, OllamaEmbedder, DEFAULT_EMBEDDING_DIM, EMBEDDING_DIM_ENV,
    EMBED_FORWARD_BATCH,
};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

/// Reads one HTTP/1.1 request off `stream` (headers + `Content-Length`
/// body) and returns the body.
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

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Starts a fake Ollama server that answers exactly `responses.len()`
/// requests in order, one raw HTTP response body per connection (always
/// `200 OK`), and returns the `http://127.0.0.1:PORT` URL to point an
/// `OllamaEmbedder` at plus a handle yielding every request body it saw, in
/// arrival order.
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

/// Same as `fake_server`, but every response carries the given status line
/// (e.g. `"500 Internal Server Error"`) instead of `200 OK`.
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

fn embed_response(vectors: &[Vec<f32>]) -> String {
    serde_json::json!({ "embeddings": vectors }).to_string()
}

#[test]
fn embed_returns_l2_normalized_vectors_parsed_from_the_response() {
    let (url, handle) = fake_server(vec![embed_response(&[vec![3.0, 4.0]])]);
    let embedder = OllamaEmbedder::new("nomic-embed-text", url, 2);

    let vectors = embedder
        .embed(&["hello".to_string()], EmbedRole::Passage)
        .unwrap();

    assert_eq!(vectors.len(), 1);
    let norm: f32 = vectors[0].iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-5,
        "expected a unit vector, got norm {norm}"
    );
    assert!((vectors[0][0] - 0.6).abs() < 1e-5);
    assert!((vectors[0][1] - 0.8).abs() < 1e-5);
    handle.join().unwrap();
}

#[test]
fn embed_sends_the_models_role_prefix_and_the_configured_model_name() {
    let (url, handle) = fake_server(vec![embed_response(&[vec![1.0, 0.0]])]);
    let embedder = OllamaEmbedder::new("nomic-embed-text", url, 2);

    embedder
        .embed(&["find the quokka".to_string()], EmbedRole::Query)
        .unwrap();

    let seen = handle.join().unwrap();
    let sent: serde_json::Value = serde_json::from_str(&seen[0]).unwrap();
    assert_eq!(sent["model"], "nomic-embed-text");
    assert_eq!(sent["input"][0], "search_query: find the quokka");
}

#[test]
fn embed_of_an_empty_batch_returns_empty_without_any_network_call() {
    // Points at a URL nothing is listening on -- a network call here would
    // fail loudly, so success proves none was attempted.
    let embedder = OllamaEmbedder::new(
        "nomic-embed-text",
        "http://127.0.0.1:1",
        DEFAULT_EMBEDDING_DIM,
    );

    let vectors = embedder.embed(&[], EmbedRole::Passage).unwrap();

    assert!(vectors.is_empty());
}

#[test]
fn a_non_2xx_status_is_a_clear_error_naming_the_status_code() {
    let (url, handle) = fake_server_with_status("500 Internal Server Error", "model not found");
    let embedder = OllamaEmbedder::new("nomic-embed-text", url, 2);

    let err = embedder
        .embed(&["text".to_string()], EmbedRole::Passage)
        .unwrap_err();

    assert!(
        err.to_string().contains("500"),
        "error should name the status: {err}"
    );
    handle.join().unwrap();
}

#[test]
fn a_dimension_mismatch_is_a_clear_error_naming_the_dim_env_var() {
    let (url, handle) = fake_server(vec![embed_response(&[vec![1.0, 0.0, 0.0]])]);
    let embedder = OllamaEmbedder::new("nomic-embed-text", url, 2);

    let err = embedder
        .embed(&["text".to_string()], EmbedRole::Passage)
        .unwrap_err();

    assert!(
        err.to_string().contains(EMBEDDING_DIM_ENV),
        "error should point at the env var to fix: {err}"
    );
    handle.join().unwrap();
}

#[test]
fn an_unreachable_server_is_a_clear_connection_error_not_a_panic() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener); // frees the port without anything ever listening on it

    let embedder = OllamaEmbedder::new("nomic-embed-text", format!("http://127.0.0.1:{port}"), 2);

    let err = embedder
        .embed(&["text".to_string()], EmbedRole::Passage)
        .unwrap_err();

    assert!(!err.to_string().is_empty());
}

#[test]
fn a_batch_larger_than_the_forward_batch_size_is_split_across_multiple_requests() {
    let texts: Vec<String> = (0..EMBED_FORWARD_BATCH + 5)
        .map(|i| format!("text {i}"))
        .collect();
    let responses = vec![
        embed_response(&vec![vec![1.0, 0.0]; EMBED_FORWARD_BATCH]),
        embed_response(&vec![vec![0.0, 1.0]; 5]),
    ];
    let (url, handle) = fake_server(responses);
    let embedder = OllamaEmbedder::new("nomic-embed-text", url, 2);

    let vectors = embedder.embed(&texts, EmbedRole::Passage).unwrap();

    assert_eq!(
        vectors.len(),
        EMBED_FORWARD_BATCH + 5,
        "all vectors across both requests are returned, in order"
    );
    let seen = handle.join().unwrap();
    assert_eq!(
        seen.len(),
        2,
        "the batch was split into exactly two HTTP requests"
    );
}
