//! Hand-rolled HTTP, matching this workspace's other servers.
//!
//! No framework, for the same reason `sync/server.rs`, `webhook.rs` and the
//! API server have none: the surface is a handful of fixed paths with query
//! strings, and every hand-rolled HTTP surface in this workspace is
//! self-contained rather than sharing a layer none of them quite fits.
//!
//! The reference gets this from FastAPI and then spends real effort turning
//! parts of it *off* — `docs_url`, `redoc_url` and `openapi_url` are all
//! disabled there, because they default to ON and UNAUTHENTICATED and would
//! publish every route, including the one that hard-deletes rows, to anyone
//! who can reach the port. Here there is nothing to disable: a route that was
//! not written does not exist.

use std::io::{self, Read, Write};

/// Cap on a request body. Matches the peer server's.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// A parsed request line and headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: String,
    pub path: String,
    pub query: String,
    pub authorization: String,
    pub content_length: Option<usize>,
}

#[derive(Debug)]
pub enum HeadOutcome {
    Parsed(Head, Vec<u8>),
    /// Malformed, oversized, or a closed connection.
    Rejected(u16, &'static str),
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Read and parse the request head, returning any body bytes already buffered.
pub fn read_head<R: Read>(stream: &mut R) -> io::Result<HeadOutcome> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(end) = find_head_end(&buf) {
            break end;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Ok(HeadOutcome::Rejected(431, "header too large"));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(HeadOutcome::Rejected(400, "connection closed mid-header"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head_text = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head_text.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Ok(HeadOutcome::Rejected(400, "empty request"));
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(HeadOutcome::Rejected(400, "malformed request line"));
    };
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut authorization = String::new();
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // Header names are case-insensitive; a client sending
        // `authorization:` lowercase must not silently fail auth.
        match name.trim().to_ascii_lowercase().as_str() {
            "authorization" => authorization = value.to_string(),
            "content-length" => content_length = value.parse::<usize>().ok(),
            _ => {}
        }
    }

    Ok(HeadOutcome::Parsed(
        Head {
            method: method.to_string(),
            path,
            query,
            authorization,
            content_length,
        },
        buf[head_end..].to_vec(),
    ))
}

/// Read the declared body, reusing whatever the head read already buffered.
pub fn read_body<R: Read>(
    stream: &mut R,
    head: &Head,
    already: Vec<u8>,
) -> io::Result<Result<Vec<u8>, (u16, &'static str)>> {
    let Some(declared) = head.content_length else {
        // No Content-Length: nothing to read. Chunked bodies are not accepted,
        // and no client of this protocol sends one.
        return Ok(Ok(Vec::new()));
    };
    if declared > MAX_BODY_BYTES {
        return Ok(Err((413, "request body too large")));
    }
    let mut body = already;
    body.truncate(declared.min(body.len()));
    let mut chunk = [0u8; 8192];
    while body.len() < declared {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        let want = declared - body.len();
        body.extend_from_slice(&chunk[..n.min(want)]);
    }
    Ok(Ok(body))
}

pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// A response ready to write.
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    /// The reference's error shape: `{"detail": "..."}`, which is what
    /// FastAPI's `HTTPException` produces and what clients already parse.
    pub fn error(status: u16, detail: impl Into<String>) -> Self {
        Self::json(status, &serde_json::json!({ "detail": detail.into() }))
    }

    pub fn text(status: u16, content_type: &'static str, body: String) -> Self {
        Self {
            status,
            content_type,
            body: body.into_bytes(),
        }
    }
}

/// Write a response, stamping the hub version on every one.
///
/// Every response, including errors, deliberately. A client doing ordinary
/// work would otherwise have to make a second, unrelated request to learn
/// which build answered it — so the version would be readable exactly where it
/// was least needed. A 401 or a 500 is when "which build is this?" matters
/// most, and those carry no JSON body with a version in it.
pub fn write_response<W: Write>(stream: &mut W, response: &Response) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         X-Hub-Version: {}\r\n\
         Connection: close\r\n\r\n",
        response.status,
        reason_phrase(response.status),
        response.content_type,
        response.body.len(),
        crate::HUB_VERSION,
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

/// Percent-decode a query value, treating `+` as a space.
pub fn urldecode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// First value for `key` in a query string, percent-decoded.
pub fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| urldecode(value))
    })
}

/// A query flag, using the truthy spellings the reference's clients send.
///
/// FastAPI accepts `true`/`1`/`yes`/`on` for a `bool` query parameter, so a
/// client written against the reference may legitimately send any of them.
pub fn query_flag(query: &str, key: &str) -> bool {
    query_param(query, key).is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Head {
        let mut cursor = io::Cursor::new(raw.as_bytes().to_vec());
        match read_head(&mut cursor).unwrap() {
            HeadOutcome::Parsed(head, _) => head,
            other => panic!("expected a parsed head, got {other:?}"),
        }
    }

    #[test]
    fn a_request_line_splits_path_from_query() {
        let head = parse("GET /sync/pull?since=x&limit=5 HTTP/1.1\r\nHost: h\r\n\r\n");
        assert_eq!(head.method, "GET");
        assert_eq!(head.path, "/sync/pull");
        assert_eq!(head.query, "since=x&limit=5");
    }

    #[test]
    fn header_names_are_matched_case_insensitively() {
        // A client sending lowercase `authorization` must not silently 401.
        let head = parse("GET /health HTTP/1.1\r\nauthorization: Bearer s\r\n\r\n");
        assert_eq!(head.authorization, "Bearer s");
    }

    #[test]
    fn an_oversized_header_is_rejected_rather_than_buffered() {
        let filler = "x".repeat(MAX_HEAD_BYTES + 100);
        let raw = format!("GET /health HTTP/1.1\r\nX-Big: {filler}\r\n");
        let mut cursor = io::Cursor::new(raw.into_bytes());
        assert!(matches!(
            read_head(&mut cursor).unwrap(),
            HeadOutcome::Rejected(431, _)
        ));
    }

    #[test]
    fn a_body_larger_than_the_cap_is_refused_before_reading_it() {
        let head = Head {
            method: "POST".into(),
            path: "/sync/push".into(),
            query: String::new(),
            authorization: String::new(),
            content_length: Some(MAX_BODY_BYTES + 1),
        };
        let mut cursor = io::Cursor::new(Vec::new());
        assert_eq!(
            read_body(&mut cursor, &head, Vec::new()).unwrap(),
            Err((413, "request body too large"))
        );
    }

    #[test]
    fn percent_and_plus_encoding_both_decode() {
        assert_eq!(urldecode("a%2Bb+c"), "a+b c");
        assert_eq!(
            urldecode("2026-08-05T12%3A00%3A00%2B00%3A00"),
            "2026-08-05T12:00:00+00:00"
        );
    }

    #[test]
    fn query_flags_accept_every_spelling_fastapi_does() {
        for truthy in ["1", "true", "TRUE", "yes", "on"] {
            assert!(
                query_flag(&format!("full={truthy}"), "full"),
                "{truthy} should be truthy"
            );
        }
        for falsy in ["0", "false", "no", ""] {
            assert!(
                !query_flag(&format!("full={falsy}"), "full"),
                "{falsy} should be falsy"
            );
        }
        assert!(!query_flag("", "full"));
    }

    #[test]
    fn every_response_carries_the_version_header_including_errors() {
        // The property the reference uses middleware to guarantee.
        let mut out = Vec::new();
        write_response(&mut out, &Response::error(401, "unauthorized")).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(&format!("X-Hub-Version: {}", crate::HUB_VERSION)),
            "{text}"
        );
        assert!(text.starts_with("HTTP/1.1 401 Unauthorized"), "{text}");
    }
}
