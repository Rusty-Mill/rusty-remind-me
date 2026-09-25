//! A minimal hand-rolled HTTP/1.1 client for talking to a hub's peer
//! server — the same `std::net::TcpStream`-based approach already used for
//! the Ollama embedding client and the webhook/HTTP-API servers, rather
//! than pulling in an async HTTP client for two request shapes.
//!
//! `http://` only, matching the same simplifying choice `embedder.rs`'s
//! `OllamaEmbedder` already made: a sync deployment that needs TLS puts a
//! reverse proxy in front, the same way an Ollama deployment would.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct HttpError(pub String);

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for HttpError {}

struct ParsedUrl {
    host: String,
    port: u16,
    path_and_query: String,
}

fn parse_url(url: &str) -> Result<ParsedUrl, HttpError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| HttpError(format!("only http:// sync URLs are supported, got {url:?}")))?;
    let (authority, path_and_query) = match rest.find('/') {
        Some(pos) => (&rest[..pos], rest[pos..].to_string()),
        None => (rest, "/".to_string()),
    };
    // A sync peer is always addressed with an explicit port, but a webhook
    // URL (`notifications.rs`) usually is not, so an absent port falls back to
    // 80 rather than being rejected.
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .map_err(|_| HttpError(format!("invalid port in URL: {url:?}")))?,
        ),
        None => (authority, 80),
    };
    Ok(ParsedUrl {
        host: host.to_string(),
        port,
        path_and_query,
    })
}

/// One HTTP request/response round-trip. `body: None` sends a bare
/// request line (used for `GET`); `Some(json)` sends it with a
/// `Content-Type: application/json` body (used for `POST`).
fn request(
    method: &str,
    url: &str,
    secret: Option<&str>,
    body: Option<&str>,
) -> Result<(u16, String), HttpError> {
    let parsed = parse_url(url)?;
    use std::net::ToSocketAddrs;
    let addr = (parsed.host.as_str(), parsed.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| HttpError(format!("cannot resolve sync host {:?}", parsed.host)))?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| HttpError(format!("cannot reach {}: {}", url, e)))?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();

    // Omitted entirely rather than sent empty when there is no secret: a bare
    // `Authorization: Bearer` is a malformed credential, and some endpoints
    // reject it outright where they would have accepted no header at all.
    let auth = match secret {
        Some(secret) => format!("Authorization: Bearer {secret}\r\n"),
        None => String::new(),
    };
    let request_text = match body {
        Some(body) => format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\n{auth}\
             Content-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            method = method,
            path = parsed.path_and_query,
            host = parsed.host,
            auth = auth,
            len = body.len(),
            body = body,
        ),
        None => format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n",
            method = method,
            path = parsed.path_and_query,
            host = parsed.host,
            auth = auth,
        ),
    };
    stream
        .write_all(request_text.as_bytes())
        .map_err(|e| HttpError(format!("writing to {}: {}", url, e)))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| HttpError(format!("reading from {}: {}", url, e)))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, response_body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| HttpError(format!("malformed HTTP response from {url}")))?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| HttpError(format!("malformed HTTP status line from {url}")))?;
    Ok((status, response_body.to_string()))
}

pub fn post_json(url: &str, secret: &str, body: &str) -> Result<(u16, String), HttpError> {
    request("POST", url, Some(secret), Some(body))
}

pub fn get(url: &str, secret: &str) -> Result<(u16, String), HttpError> {
    request("GET", url, Some(secret), None)
}

/// POST JSON with no `Authorization` header, for an endpoint this node does
/// not share a secret with — a user-configured notification webhook, where
/// any credential belongs in the URL the service itself issued.
pub fn post_json_unauthenticated(url: &str, body: &str) -> Result<(u16, String), HttpError> {
    request("POST", url, None, Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_splits_host_port_and_path_with_query() {
        let parsed = parse_url("http://example.com:8766/sync/pull?since=x&limit=5").unwrap();
        assert_eq!(parsed.host, "example.com");
        assert_eq!(parsed.port, 8766);
        assert_eq!(parsed.path_and_query, "/sync/pull?since=x&limit=5");
    }

    #[test]
    fn parse_url_defaults_to_root_path() {
        let parsed = parse_url("http://example.com:8766").unwrap();
        assert_eq!(parsed.path_and_query, "/");
    }

    #[test]
    fn parse_url_defaults_a_missing_port_to_80() {
        // Sync peers always carry an explicit port; webhook URLs usually do
        // not, and rejecting them would make the notifier unusable against
        // every ordinary endpoint.
        let parsed = parse_url("http://example.com/foo").unwrap();
        assert_eq!(parsed.host, "example.com");
        assert_eq!(parsed.port, 80);
        assert_eq!(parsed.path_and_query, "/foo");
    }

    #[test]
    fn parse_url_rejects_https() {
        assert!(parse_url("https://example.com:8766/foo").is_err());
    }
}
