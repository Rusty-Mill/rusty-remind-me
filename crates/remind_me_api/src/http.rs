//! Minimal HTTP/1.1 request parsing and response writing.
//!
//! One connection at a time, synchronous, over `std::net` — the same shape as
//! [`remind_me_core::webhook`]'s protocol handling, generalized here for
//! multiple routes, methods, path parameters and query strings rather than a
//! single fixed endpoint. The token comparison itself is *not*
//! reimplemented — [`remind_me_core::webhook::constant_time_eq`] is reused
//! directly, since a second, independent bearer-auth comparison next to the
//! webhook's is exactly the drift-on-a-security-boundary risk called out
//! against this crate's own history.
//!
//! Every request is read and answered fully before the next connection is
//! accepted. Every handler takes the database lock to do anything useful, so
//! a thread per connection would not make requests finish sooner — it would
//! only move the queue out of the kernel's accept backlog and into unbounded
//! threads inside this process.

use std::collections::HashMap;
use std::io::{self, Read, Write};

/// Largest request body accepted, before anything is buffered.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Largest request line plus header block accepted.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;

/// A parsed request: method, decoded path, decoded query parameters, the
/// headers this layer cares about, and the body.
#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// Decoded, without the query string.
    pub path: String,
    pub query: HashMap<String, String>,
    pub authorization: String,
    pub content_type: String,
    /// The `Origin` header, empty when absent -- what a CORS decision is
    /// made against. Only ever sent by a browser; a direct `curl`/script
    /// caller has none, which is fine: CORS is a browser-enforced policy,
    /// nothing here needs it to work non-interactively.
    pub origin: String,
    pub body: Vec<u8>,
}

impl Request {
    /// A query parameter, or `None` if absent or blank.
    pub fn query_str(&self, name: &str) -> Option<&str> {
        self.query
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// A query parameter parsed as an integer.
    ///
    /// `Ok(default)` when absent or blank; `Err` carries a client-facing
    /// message so a handler can answer 400 instead of panicking on garbage
    /// input — the same reasoning as the reference's `_int_param`.
    pub fn query_usize(&self, name: &str, default: usize) -> Result<usize, String> {
        match self.query_str(name) {
            None => Ok(default),
            Some(raw) => raw
                .parse()
                .map_err(|_| format!("Invalid integer for query parameter '{}': {:?}", name, raw)),
        }
    }

    /// A comma-separated query parameter, or `None` if absent or blank.
    pub fn query_list(&self, name: &str) -> Option<Vec<String>> {
        self.query_str(name)
            .map(|raw| raw.split(',').map(str::to_string).collect())
    }

    /// A boolean-ish query parameter (`false`/`0`/`no` are false; anything
    /// else, including absent, is true) — matches the reference's
    /// `include_index` parsing exactly.
    pub fn query_bool_default_true(&self, name: &str) -> bool {
        match self.query_str(name) {
            None => true,
            Some(raw) => !matches!(raw.to_ascii_lowercase().as_str(), "false" | "0" | "no"),
        }
    }

    /// A flag that is off unless explicitly asked for.
    ///
    /// The mirror of [`Self::query_bool_default_true`], and deliberately not
    /// its negation: a bare `?flag` with no value reads as *on* here, which is
    /// how a query-string flag is normally written.
    pub fn query_bool_default_false(&self, name: &str) -> bool {
        match self.query_str(name) {
            None => false,
            Some("") => true,
            Some(raw) => matches!(raw.to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
        }
    }
}

/// Percent-decode a query string component, and turn `+` into a space —
/// `application/x-www-form-urlencoded` rules, which is what a browser or
/// `curl --data-urlencode` produces for a query string.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a `key=value&key2=value2` query string.
///
/// A key with no `=` is recorded with an empty value rather than dropped, so
/// `?dry_run` and `?dry_run=` both parse (even though every route here in
/// practice expects `=`).
fn parse_query(raw: &str) -> HashMap<String, String> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// What the `Content-Length` header said.
///
/// "Absent" and "present but unparseable" get different responses, so they
/// are not collapsed into an `Option`.
#[derive(Debug, PartialEq, Eq)]
pub enum ContentLength {
    Absent,
    Invalid,
    Value(usize),
}

/// Boxed so `HeadOutcome` (whose other variants carry nothing) stays small —
/// this struct is the one variant with real data, not a shared type used
/// elsewhere.
struct CompleteHead {
    method: String,
    path: String,
    query: HashMap<String, String>,
    authorization: String,
    content_type: String,
    origin: String,
    content_length: ContentLength,
    body_prefix: Vec<u8>,
}

enum HeadOutcome {
    Complete(Box<CompleteHead>),
    TooLarge,
    /// The client hung up, or sent something that is not an HTTP request line.
    Unusable,
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn read_head<R: Read>(stream: &mut R) -> io::Result<HeadOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    let split = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Ok(HeadOutcome::TooLarge);
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(HeadOutcome::Unusable);
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let body_prefix = buf.split_off(split + 4);
    buf.truncate(split);

    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");

    let Some(request_line) = lines.next() else {
        return Ok(HeadOutcome::Unusable);
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(HeadOutcome::Unusable);
    };
    if method.is_empty() || target.is_empty() {
        return Ok(HeadOutcome::Unusable);
    }

    let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let path = percent_decode(raw_path.split('#').next().unwrap_or(raw_path));
    let query = parse_query(raw_query.split('#').next().unwrap_or(raw_query));

    let mut authorization = String::new();
    let mut content_type = String::new();
    let mut origin = String::new();
    let mut content_length = ContentLength::Absent;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "authorization" => authorization = value.to_string(),
            "content-type" => content_type = value.to_string(),
            "origin" => origin = value.to_string(),
            "content-length" => {
                content_length = match value.parse::<usize>() {
                    Ok(n) => ContentLength::Value(n),
                    Err(_) => ContentLength::Invalid,
                }
            }
            _ => {}
        }
    }

    Ok(HeadOutcome::Complete(Box::new(CompleteHead {
        method: method.to_string(),
        path,
        query,
        authorization,
        content_type,
        origin,
        content_length,
        body_prefix,
    })))
}

/// Read and discard a pending body before an early rejection.
///
/// Closing a connection with unread bytes still in the receive buffer makes
/// the peer see a connection reset instead of the response just written.
/// Bounded by [`MAX_BODY_BYTES`] regardless of what the client declared —
/// this can run before any check of the declared length.
fn drain_body<R: Read>(stream: &mut R, declared: &ContentLength, already_read: usize) {
    let ContentLength::Value(length) = declared else {
        return;
    };
    let mut remaining = (*length).min(MAX_BODY_BYTES).saturating_sub(already_read);
    let mut chunk = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) | Err(_) => return,
            Ok(read) => remaining -= read,
        }
    }
}

/// The reference's own CORS policy, confirmed directly from
/// `remind_me_mcp/api.py`'s `CORSMiddleware` setup
/// (`allow_origin_regex=r"http://(localhost|127\.0\.0\.1)(:\d+)?"`,
/// `allow_methods=["*"]`, `allow_headers=["*"]`) rather than assumed.
///
/// `None` when `origin` is empty (no `Origin` header — not a browser
/// request, CORS is irrelevant) or doesn't match; a caller adds no CORS
/// headers in that case, which is what makes a non-matching cross-origin
/// browser request fail closed rather than silently wide open.
pub fn cors_allowed_origin(origin: &str) -> Option<&str> {
    let rest = origin.strip_prefix("http://")?;
    let (host, port) = rest.split_once(':').unzip();
    let host = host.unwrap_or(rest);
    if host != "localhost" && host != "127.0.0.1" {
        return None;
    }
    if let Some(port) = port {
        port.parse::<u16>().ok()?;
    }
    Some(origin)
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

/// A response body: JSON, or a raw payload with its own content type (for
/// `GET /api/export`, which returns the export file's own bytes rather than
/// a JSON envelope when writing inline).
pub enum Body {
    Json(serde_json::Value),
    Raw {
        content_type: &'static str,
        payload: String,
    },
}

pub fn write_response<W: Write>(stream: &mut W, status: u16, body: Body) -> io::Result<()> {
    write_response_cors(stream, status, body, None)
}

/// As [`write_response`], additionally reflecting `cors_origin` (the result
/// of [`cors_allowed_origin`]) as `Access-Control-Allow-Origin` plus the
/// reference's own blanket `allow_methods`/`allow_headers` -- `None` adds no
/// CORS headers at all, exactly as a non-matching or absent `Origin` gets
/// from the reference's `CORSMiddleware`.
pub fn write_response_cors<W: Write>(
    stream: &mut W,
    status: u16,
    body: Body,
    cors_origin: Option<&str>,
) -> io::Result<()> {
    let (content_type, payload) = match body {
        Body::Json(value) => (
            "application/json",
            serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        ),
        Body::Raw {
            content_type,
            payload,
        } => (content_type, payload.into_bytes()),
    };
    let cors_headers = match cors_origin {
        Some(origin) => format!(
            "Access-Control-Allow-Origin: {origin}\r\n\
             Access-Control-Allow-Methods: *\r\n\
             Access-Control-Allow-Headers: *\r\n"
        ),
        None => String::new(),
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         {}\
         Connection: close\r\n\r\n",
        status,
        reason_phrase(status),
        content_type,
        payload.len(),
        cors_headers,
    )?;
    stream.write_all(&payload)?;
    stream.flush()
}

/// Read one full request off `stream` (headers, then a size-capped body).
///
/// `Ok(None)` means nothing usable arrived (a closed connection, or garbage
/// that is not an HTTP request line) — there is nothing to answer.
pub fn read_request<S: Read + Write>(stream: &mut S) -> io::Result<Option<Request>> {
    let (method, path, query, authorization, content_type, origin, content_length, body_prefix) =
        match read_head(stream)? {
            HeadOutcome::Complete(head) => (
                head.method,
                head.path,
                head.query,
                head.authorization,
                head.content_type,
                head.origin,
                head.content_length,
                head.body_prefix,
            ),
            HeadOutcome::TooLarge => {
                write_response(
                    stream,
                    431,
                    Body::Json(serde_json::json!({ "error": "request headers too large" })),
                )?;
                return Ok(None);
            }
            HeadOutcome::Unusable => return Ok(None),
        };

    let length = match content_length {
        ContentLength::Invalid => {
            write_response(
                stream,
                400,
                Body::Json(serde_json::json!({ "error": "invalid content-length" })),
            )?;
            return Ok(None);
        }
        ContentLength::Absent => 0,
        ContentLength::Value(n) if n > MAX_BODY_BYTES => {
            drain_body(stream, &content_length, body_prefix.len());
            write_response(
                stream,
                413,
                Body::Json(serde_json::json!({ "error": "request body too large" })),
            )?;
            return Ok(None);
        }
        ContentLength::Value(n) => n,
    };

    let mut body = body_prefix;
    body.truncate(length);
    let mut chunk = [0u8; 8192];
    while body.len() < length {
        let want = (length - body.len()).min(chunk.len());
        match stream.read(&mut chunk[..want])? {
            0 => break,
            read => body.extend_from_slice(&chunk[..read]),
        }
    }
    if body.len() < length {
        write_response(
            stream,
            400,
            Body::Json(serde_json::json!({ "error": "truncated request body" })),
        )?;
        return Ok(None);
    }

    Ok(Some(Request {
        method,
        path,
        query,
        authorization,
        content_type,
        origin,
        body,
    }))
}

/// Match a `/api/memories/{id}`-style pattern against a real path.
///
/// Segment-by-segment: a `{name}` segment matches anything and is captured;
/// every other segment must match literally. Returns `None` on a length or
/// literal mismatch.
pub fn match_pattern(pattern: &str, path: &str) -> Option<HashMap<String, String>> {
    let pattern_segments: Vec<&str> = pattern.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();
    if pattern_segments.len() != path_segments.len() {
        return None;
    }

    let mut params = HashMap::new();
    for (p, s) in pattern_segments.iter().zip(path_segments.iter()) {
        if let Some(name) = p.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
            if s.is_empty() {
                return None;
            }
            params.insert(name.to_string(), percent_decode(s));
        } else if p != s {
            return None;
        }
    }
    Some(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_handles_spaces_and_encoded_bytes() {
        assert_eq!(percent_decode("hello+world"), "hello world");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("100%25"), "100%");
    }

    #[test]
    fn an_incomplete_percent_escape_is_left_alone() {
        assert_eq!(percent_decode("100%2"), "100%2");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn query_parsing_decodes_keys_and_values() {
        let q = parse_query("category=chat_import&tags=a%2Cb%2Cc");
        assert_eq!(q.get("category").unwrap(), "chat_import");
        assert_eq!(q.get("tags").unwrap(), "a,b,c");
    }

    #[test]
    fn a_key_with_no_equals_gets_an_empty_value() {
        let q = parse_query("dry_run");
        assert_eq!(q.get("dry_run").unwrap(), "");
    }

    #[test]
    fn pattern_matching_captures_named_segments() {
        let params =
            match_pattern("/api/memories/{memory_id}", "/api/memories/mem_abc123").unwrap();
        assert_eq!(params.get("memory_id").unwrap(), "mem_abc123");
    }

    #[test]
    fn pattern_matching_rejects_a_length_or_literal_mismatch() {
        assert!(match_pattern("/api/memories/{id}", "/api/memories").is_none());
        assert!(match_pattern("/api/memories/{id}", "/api/memories/x/y").is_none());
        assert!(match_pattern("/api/wiki/{slug}", "/api/entities").is_none());
    }

    #[test]
    fn an_empty_captured_segment_does_not_match() {
        // "/api/memories/" must not resolve to memory_id == "".
        assert!(match_pattern("/api/memories/{id}", "/api/memories/").is_none());
    }
}
