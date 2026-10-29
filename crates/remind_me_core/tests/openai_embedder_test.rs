//! Coverage for `OpenAiEmbedder` (`/v1/embeddings`, as served by llama.cpp's
//! server and llama-swap) against a fake server on a bare `TcpListener`,
//! the same pattern `ollama_embedder_test.rs` uses.

use remind_me_core::embedder::{EmbedRole, Embedder, OpenAiEmbedder, OPENAI_EMBED_MODEL_ENV};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

/// Reads one HTTP/1.1 request off `stream`; returns its request line and body.
fn read_request(stream: &mut TcpStream) -> (String, String) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let headers_end = loop {
        let n = stream.read(&mut chunk).expect("read request");
        assert!(n > 0, "connection closed before headers completed");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
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
    let request_line = head.lines().next().unwrap_or_default().to_string();
    let body = String::from_utf8_lossy(&buf[headers_end..headers_end + content_length]).to_string();
    (request_line, body)
}

/// A fake server answering one request per response, in order, with
/// `status`; yields each request's (request line, body).
fn fake_server(
    status: &str,
    responses: Vec<String>,
) -> (String, JoinHandle<Vec<(String, String)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let status = status.to_string();
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for body in responses {
            let (mut stream, _) = listener.accept().expect("accept");
            seen.push(read_request(&mut stream));
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
        seen
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

/// A fake server answering one request with the raw HTTP response `raw`.
fn raw_server(raw: String) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        read_request(&mut stream);
        stream.write_all(raw.as_bytes()).expect("write response");
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

/// `body` as a chunked HTTP 200 response, split into `size`-byte chunks
/// (the first carrying a chunk extension), ending in `terminator`.
fn chunked(body: &str, size: usize, terminator: &str) -> String {
    let mut raw = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: Chunked\r\nConnection: close\r\n\r\n",
    );
    for (n, piece) in body.as_bytes().chunks(size).enumerate() {
        let ext = if n == 0 { ";name=value" } else { "" };
        raw.push_str(&format!("{:x}{ext}\r\n", piece.len()));
        raw.push_str(std::str::from_utf8(piece).unwrap());
        raw.push_str("\r\n");
    }
    raw.push_str(terminator);
    raw
}

/// An OpenAI-style response with `(index, vector)` entries in the given order.
fn response(entries: &[(usize, Vec<f32>)]) -> String {
    let data: Vec<_> = entries
        .iter()
        .map(|(i, v)| serde_json::json!({ "object": "embedding", "index": i, "embedding": v }))
        .collect();
    serde_json::json!({ "object": "list", "data": data, "model": "m" }).to_string()
}

#[test]
fn embeds_through_v1_embeddings_with_the_configured_model() {
    let (url, handle) = fake_server("200 OK", vec![response(&[(0, vec![3.0, 4.0])])]);
    let embedder = OpenAiEmbedder::new("qwen3-embedding-0.6b", url, 2);

    let vectors = embedder
        .embed(&["hello".to_string()], EmbedRole::Passage)
        .unwrap();

    assert!((vectors[0][0] - 0.6).abs() < 1e-5 && (vectors[0][1] - 0.8).abs() < 1e-5);
    let seen = handle.join().unwrap();
    assert!(
        seen[0].0.starts_with("POST /v1/embeddings "),
        "{}",
        seen[0].0
    );
    let sent: serde_json::Value = serde_json::from_str(&seen[0].1).unwrap();
    assert_eq!(sent["model"], "qwen3-embedding-0.6b");
    assert_eq!(sent["input"][0], "hello");
}

#[test]
fn a_trailing_v1_in_the_url_is_not_doubled() {
    let (url, handle) = fake_server("200 OK", vec![response(&[(0, vec![1.0, 0.0])])]);
    let embedder = OpenAiEmbedder::new("m", format!("{url}/v1/"), 2);

    embedder
        .embed(&["x".to_string()], EmbedRole::Query)
        .unwrap();

    assert!(handle.join().unwrap()[0]
        .0
        .starts_with("POST /v1/embeddings "));
}

#[test]
fn entries_are_placed_by_index_not_arrival_order() {
    let (url, handle) = fake_server(
        "200 OK",
        vec![response(&[(1, vec![0.0, 1.0]), (0, vec![1.0, 0.0])])],
    );
    let embedder = OpenAiEmbedder::new("m", url, 2);

    let vectors = embedder
        .embed(
            &["first".to_string(), "second".to_string()],
            EmbedRole::Passage,
        )
        .unwrap();

    assert_eq!(vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    handle.join().unwrap();
}

#[test]
fn the_wrong_number_of_embeddings_is_an_error() {
    let (url, handle) = fake_server("200 OK", vec![response(&[(0, vec![1.0, 0.0])])]);
    let embedder = OpenAiEmbedder::new("m", url, 2);

    let err = embedder
        .embed(&["a".to_string(), "b".to_string()], EmbedRole::Passage)
        .unwrap_err();

    assert!(err.0.contains("did not return 2 embeddings"), "{err}");
    handle.join().unwrap();
}

#[test]
fn a_repeated_or_out_of_range_index_is_an_error() {
    for entries in [
        vec![(0, vec![1.0, 0.0]), (0, vec![0.0, 1.0])],
        vec![(0, vec![1.0, 0.0]), (5, vec![0.0, 1.0])],
    ] {
        let (url, handle) = fake_server("200 OK", vec![response(&entries)]);
        let embedder = OpenAiEmbedder::new("m", url, 2);

        let err = embedder
            .embed(&["a".to_string(), "b".to_string()], EmbedRole::Passage)
            .unwrap_err();

        assert!(err.0.contains("bad index"), "{err}");
        handle.join().unwrap();
    }
}

#[test]
fn a_missing_or_malformed_index_is_an_error() {
    for index in [
        None,
        Some(serde_json::json!(null)),
        Some(serde_json::json!("0")),
        Some(serde_json::json!(0.5)),
        Some(serde_json::json!(-1)),
    ] {
        let mut entry = serde_json::json!({ "embedding": [1.0, 0.0] });
        if let Some(index) = &index {
            entry["index"] = index.clone();
        }
        let body = serde_json::json!({ "data": [entry] }).to_string();
        let (url, handle) = fake_server("200 OK", vec![body]);
        let embedder = OpenAiEmbedder::new("m", url, 2);

        let err = embedder
            .embed(&["a".to_string()], EmbedRole::Passage)
            .unwrap_err();

        assert!(err.0.contains("no valid index"), "{index:?}: {err}");
        handle.join().unwrap();
    }
}

#[test]
fn a_chunked_response_is_decoded() {
    for terminator in ["0\r\n\r\n", "0\r\nX-Trailer: v\r\nX-Other: w\r\n\r\n"] {
        let body = response(&[(1, vec![0.0, 2.0]), (0, vec![3.0, 0.0])]);
        let (url, handle) = raw_server(chunked(&body, 7, terminator));
        let embedder = OpenAiEmbedder::new("m", url, 2);

        let vectors = embedder
            .embed(&["a".to_string(), "b".to_string()], EmbedRole::Passage)
            .unwrap();

        assert_eq!(
            vectors,
            vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            "{terminator:?}"
        );
        handle.join().unwrap();
    }
}

#[test]
fn a_truncated_chunked_response_is_an_error() {
    let body = response(&[(0, vec![1.0, 0.0])]);
    let mut cut_mid_chunk = chunked(&body, 7, "");
    cut_mid_chunk.truncate(cut_mid_chunk.len() - 4);
    for raw in [
        cut_mid_chunk,
        chunked(&body, 7, "0\r\n"),
        chunked(&body, 7, "0\r\nX-Trailer: v\r\n"),
        chunked(&body, 7, "0\r\nX-Trailer: v"),
    ] {
        let (url, handle) = raw_server(raw.clone());
        let embedder = OpenAiEmbedder::new("m", url, 2);

        let err = embedder
            .embed(&["a".to_string()], EmbedRole::Passage)
            .unwrap_err();

        assert!(err.0.contains("malformed HTTP response"), "{raw:?}: {err}");
        handle.join().unwrap();
    }
}

#[test]
fn a_dimension_mismatch_names_the_dim_setting() {
    let (url, handle) = fake_server("200 OK", vec![response(&[(0, vec![1.0, 0.0, 0.0])])]);
    let embedder = OpenAiEmbedder::new("m", url, 2);

    let err = embedder
        .embed(&["a".to_string()], EmbedRole::Passage)
        .unwrap_err();

    assert!(
        err.0.contains("returned dim 3") && err.0.contains("REMIND_ME_EMBEDDING_DIM"),
        "{err}"
    );
    handle.join().unwrap();
}

#[test]
fn a_non_2xx_status_is_an_error_naming_it() {
    let (url, handle) = fake_server(
        "404 Not Found",
        vec!["{\"error\":\"no model\"}".to_string()],
    );
    let embedder = OpenAiEmbedder::new("m", url, 2);

    let err = embedder
        .embed(&["a".to_string()], EmbedRole::Passage)
        .unwrap_err();

    assert!(err.0.contains("returned 404"), "{err}");
    handle.join().unwrap();
}

#[test]
fn a_missing_model_is_an_error_before_any_request() {
    let embedder = OpenAiEmbedder::new("", "http://127.0.0.1:9", 2);

    let err = embedder
        .embed(&["a".to_string()], EmbedRole::Passage)
        .unwrap_err();

    assert!(err.0.contains(OPENAI_EMBED_MODEL_ENV), "{err}");
}

#[test]
fn identity_names_the_openai_backend() {
    let identity = OpenAiEmbedder::new("m", "http://127.0.0.1:9", 2).identity();
    assert_eq!(
        (
            identity.backend.as_str(),
            identity.model.as_str(),
            identity.dim
        ),
        ("openai", "m", 2)
    );
}

#[test]
fn qwen3_embedding_queries_carry_its_instruction_and_passages_go_bare() {
    let (url, handle) = fake_server(
        "200 OK",
        vec![
            response(&[(0, vec![1.0, 0.0])]),
            response(&[(0, vec![1.0, 0.0])]),
        ],
    );
    let embedder = OpenAiEmbedder::new("Qwen3-Embedding-0.6B-Q8_0", url, 2);

    embedder
        .embed(&["quokkas".to_string()], EmbedRole::Query)
        .unwrap();
    embedder
        .embed(&["quokkas".to_string()], EmbedRole::Passage)
        .unwrap();

    let seen = handle.join().unwrap();
    let query: serde_json::Value = serde_json::from_str(&seen[0].1).unwrap();
    let passage: serde_json::Value = serde_json::from_str(&seen[1].1).unwrap();
    let query = query["input"][0].as_str().unwrap();
    assert!(
        query.starts_with("Instruct: ") && query.ends_with("\nQuery:quokkas"),
        "{query}"
    );
    assert_eq!(passage["input"][0], "quokkas");
}
