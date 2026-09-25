//! Optional OpenTelemetry tracing — off by default and zero-cost when unset,
//! matching `remind_me_mcp/telemetry.py`'s `maybe_span()` exactly in shape:
//! a context-manager-like guard that is a genuine no-op whenever tracing is
//! disabled or setup fails, so telemetry can never break the thing it's
//! observing.
//!
//! Instrumented at the same four boundaries the reference names, deliberately
//! not more: every MCP tool call, each sync cycle, each folder-watcher scan
//! pass, each webhook ingest request.
//!
//! # Deliberate deviation: a background exporter thread, not a batch processor
//!
//! The reference builds a real OTEL SDK `TracerProvider` with a
//! `BatchSpanProcessor` — spans queue in-process and a background thread
//! flushes them in batches. This crate has no OTEL SDK dependency (adding
//! one would mean either the full `opentelemetry`/`opentelemetry-otlp` crate
//! family, which pulls in an async runtime this crate has consistently
//! avoided everywhere else — see `sync/http.rs`, `embedder.rs` — or a
//! hand-rolled equivalent that reimplements most of the SDK just to get
//! batching). Emitting a real, spec-conformant OTLP/HTTP JSON payload
//! ([confirmed against the OpenTelemetry spec directly](https://opentelemetry.io/docs/specs/otlp/),
//! not guessed) is the part that actually matters for interop with a real
//! collector — batching is an optimization on top of that, not a
//! correctness requirement. So: one span per HTTP POST, sent from a
//! dedicated background thread over a bounded channel (the same
//! `std::thread::Builder::new().spawn(...)` shape `SyncWorker` already
//! uses), so a slow or unreachable collector never blocks the tool call,
//! sync cycle, watcher pass, or webhook request the span is timing. A full
//! channel silently drops the span rather than blocking the caller —
//! best-effort, matching the reference's own "telemetry must never be able
//! to break the server it's observing."

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const OTEL_ENABLED_ENV: &str = "REMIND_ME_OTEL_ENABLED";
pub const OTEL_ENDPOINT_ENV: &str = "REMIND_ME_OTEL_ENDPOINT";
pub const OTEL_SERVICE_NAME_ENV: &str = "REMIND_ME_OTEL_SERVICE_NAME";

/// The real OTLP/HTTP exporter's own default when `REMIND_ME_OTEL_ENDPOINT`
/// is unset — confirmed against the reference's `OTLPSpanExporter()` (no
/// endpoint argument) and the OpenTelemetry spec's default port/path
/// (4318, `/v1/traces`), not just the bare host:port the env var's own
/// doc-comment paraphrases.
pub const DEFAULT_OTEL_ENDPOINT: &str = "http://localhost:4318/v1/traces";
pub const DEFAULT_SERVICE_NAME: &str = "remind-me-mcp";

const CHANNEL_CAPACITY: usize = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

fn otel_enabled() -> bool {
    matches!(
        std::env::var(OTEL_ENABLED_ENV)
            .map(|v| v.to_lowercase())
            .as_deref(),
        Ok("true") | Ok("1") | Ok("yes")
    )
}

fn otel_endpoint() -> String {
    std::env::var(OTEL_ENDPOINT_ENV)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_OTEL_ENDPOINT.to_string())
}

fn otel_service_name() -> String {
    std::env::var(OTEL_SERVICE_NAME_ENV)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string())
}

/// Permanently latched on the exporter thread's first failed export —
/// matching the reference's `_get_tracer()` disabling itself "for the rest
/// of the run" on any failure (bad endpoint, SDK error). Checked before the
/// exporter thread's `OnceLock`, so once tripped every later `maybe_span`
/// call is a plain no-op, not just a failed send.
static TRACING_DISABLED: AtomicBool = AtomicBool::new(false);
static EXPORTER: OnceLock<Option<SyncSender<FinishedSpan>>> = OnceLock::new();

/// The exporter thread's last failure, if tracing has latched off — this
/// crate has no logging-framework dependency (matching every other
/// background worker here, e.g. `SyncWorkerStatus::last_error`), so a
/// queryable status field is how a caller finds out *why* tracing stopped,
/// rather than a log line nothing may be watching.
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// The reason tracing latched off, if it has. `None` while tracing is
/// disabled, still starting up, or exporting successfully.
pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

struct FinishedSpan {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    name: String,
    start: SystemTime,
    end: SystemTime,
    is_error: bool,
}

/// True once tracing is both enabled and its exporter thread is up —
/// mirrors the reference's `is_enabled()` (used by
/// `remind_me_server_status`). Triggers the same lazy, once-only
/// initialization as [`maybe_span`].
pub fn is_enabled() -> bool {
    sender().is_some()
}

fn sender() -> Option<&'static SyncSender<FinishedSpan>> {
    if TRACING_DISABLED.load(Ordering::Relaxed) {
        return None;
    }
    EXPORTER
        .get_or_init(|| {
            if !otel_enabled() {
                return None;
            }
            let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
            std::thread::Builder::new()
                .name("otel-exporter".to_string())
                .spawn(move || run_exporter(rx))
                .ok()
                .map(|_| tx)
        })
        .as_ref()
}

fn run_exporter(rx: Receiver<FinishedSpan>) {
    let endpoint = otel_endpoint();
    let service_name = otel_service_name();
    for span in rx {
        if let Err(e) = export_span(&endpoint, &service_name, &span) {
            *LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
            TRACING_DISABLED.store(true, Ordering::Relaxed);
            return;
        }
    }
}

/// An open span, or a no-op when tracing is off/unavailable — the same
/// "call `maybe_span`, get a guard, do nothing further" shape the
/// reference's context manager gives every call site. Dropping the guard
/// closes the span; [`Span::mark_error`] before dropping records that the
/// operation it timed failed.
pub struct Span(Option<OpenSpan>);

struct OpenSpan {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    name: String,
    start: SystemTime,
    is_error: bool,
}

/// Open a span named `name` (e.g. `"tool.remind_me_search"`, `"sync.cycle"`,
/// `"watcher.scan"`, `"webhook.ingest"`), or a no-op guard when tracing is
/// disabled/unavailable/latched-off. Always returns a real value (never
/// `Option<Span>`) so a call site never has to branch on whether tracing is
/// on — exactly what makes this safe to sprinkle at every boundary.
pub fn maybe_span(name: &str) -> Span {
    if sender().is_none() {
        return Span(None);
    }
    Span(Some(OpenSpan {
        trace_id: *Uuid::new_v4().as_bytes(),
        span_id: Uuid::new_v4().as_bytes()[..8].try_into().expect("8 bytes"),
        name: name.to_string(),
        start: SystemTime::now(),
        is_error: false,
    }))
}

impl Span {
    /// Record that the operation this span is timing failed. A no-op guard
    /// (tracing disabled) silently ignores this, matching the reference's
    /// `record_exception` only ever mattering when a real span is open.
    pub fn mark_error(&mut self) {
        if let Some(open) = &mut self.0 {
            open.is_error = true;
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let Some(open) = self.0.take() else { return };
        let Some(tx) = sender() else { return };
        let _ = tx.try_send(FinishedSpan {
            trace_id: open.trace_id,
            span_id: open.span_id,
            name: open.name,
            start: open.start,
            end: SystemTime::now(),
            is_error: open.is_error,
        });
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn unix_nanos(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// The OTLP/HTTP JSON trace-export payload for one span — shape confirmed
/// directly against the OpenTelemetry spec
/// (`resourceSpans`/`scopeSpans`/`spans`, hex-encoded ids, `startTimeUnixNano`/
/// `endTimeUnixNano` as decimal-string nanoseconds, integer `kind`/`status.code`
/// enums), not guessed from the reference's Python SDK usage alone.
fn span_payload(service_name: &str, span: &FinishedSpan) -> serde_json::Value {
    serde_json::json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [
                    { "key": "service.name", "value": { "stringValue": service_name } }
                ]
            },
            "scopeSpans": [{
                "scope": { "name": "rusty_remind_me" },
                "spans": [{
                    "traceId": hex_encode(&span.trace_id),
                    "spanId": hex_encode(&span.span_id),
                    "name": span.name,
                    "kind": 1, // SPAN_KIND_INTERNAL -- matches the reference's own untyped default
                    "startTimeUnixNano": unix_nanos(span.start).to_string(),
                    "endTimeUnixNano": unix_nanos(span.end).to_string(),
                    "status": { "code": if span.is_error { 2 } else { 1 } } // ERROR : OK
                }]
            }]
        }]
    })
}

#[derive(Debug)]
struct ExportError(String);

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ExportError {}

struct ParsedUrl {
    host: String,
    port: u16,
    path: String,
}

/// `http://` only, matching the same simplifying choice every other
/// hand-rolled client in this crate already makes (`sync/http.rs`,
/// `embedder.rs`): a deployment that needs TLS to reach its collector puts
/// a reverse proxy in front.
fn parse_url(url: &str) -> Result<ParsedUrl, ExportError> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        ExportError(format!(
            "only http:// OTLP endpoints are supported, got {url:?}"
        ))
    })?;
    let (authority, path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], rest[pos..].to_string()),
        None => (rest, "/v1/traces".to_string()),
    };
    let (host, port) = authority
        .split_once(':')
        .ok_or_else(|| ExportError(format!("OTLP endpoint has no port: {url:?}")))?;
    let port: u16 = port
        .parse()
        .map_err(|_| ExportError(format!("invalid port in OTLP endpoint: {url:?}")))?;
    Ok(ParsedUrl {
        host: host.to_string(),
        port,
        path,
    })
}

fn export_span(endpoint: &str, service_name: &str, span: &FinishedSpan) -> Result<(), ExportError> {
    let parsed = parse_url(endpoint)?;
    let body = span_payload(service_name, span).to_string();

    let addr = (parsed.host.as_str(), parsed.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| {
            ExportError(format!(
                "cannot resolve OTLP collector host {:?}",
                parsed.host
            ))
        })?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| ExportError(format!("cannot reach {}: {}", endpoint, e)))?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        path = parsed.path,
        host = parsed.host,
        len = body.len(),
        body = body,
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| ExportError(format!("writing to {}: {}", endpoint, e)))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| ExportError(format!("reading from {}: {}", endpoint, e)))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, _) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| ExportError(format!("malformed HTTP response from {endpoint}")))?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| ExportError(format!("malformed HTTP status line from {endpoint}")))?;

    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(ExportError(format!(
            "OTLP collector at {endpoint} returned {status}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encode_matches_the_otlp_spec_shape() {
        assert_eq!(hex_encode(&[0x5b, 0x8e, 0xff, 0xf7]), "5b8efff7");
    }

    #[test]
    fn parse_url_defaults_to_the_traces_path_when_bare() {
        let parsed = parse_url("http://localhost:4318").unwrap();
        assert_eq!(parsed.host, "localhost");
        assert_eq!(parsed.port, 4318);
        assert_eq!(parsed.path, "/v1/traces");
    }

    #[test]
    fn parse_url_keeps_an_explicit_path() {
        let parsed = parse_url("http://collector:4318/v1/traces").unwrap();
        assert_eq!(parsed.path, "/v1/traces");
    }

    #[test]
    fn parse_url_rejects_https() {
        assert!(parse_url("https://localhost:4318/v1/traces").is_err());
    }

    #[test]
    fn span_payload_matches_the_otlp_json_schema() {
        let span = FinishedSpan {
            trace_id: [0u8; 16],
            span_id: [1u8; 8],
            name: "tool.remind_me_search".to_string(),
            start: UNIX_EPOCH + Duration::from_secs(1),
            end: UNIX_EPOCH + Duration::from_secs(2),
            is_error: false,
        };
        let payload = span_payload("remind-me-mcp", &span);
        let rs = &payload["resourceSpans"][0];
        assert_eq!(
            rs["resource"]["attributes"][0]["value"]["stringValue"],
            "remind-me-mcp"
        );
        let s = &rs["scopeSpans"][0]["spans"][0];
        // 16 trace-id bytes hex-encode to 32 chars; 8 span-id bytes to 16.
        assert_eq!(s["traceId"], hex_encode(&[0u8; 16]));
        assert_eq!(s["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(s["spanId"], hex_encode(&[1u8; 8]));
        assert_eq!(s["spanId"].as_str().unwrap().len(), 16);
        assert_eq!(s["name"], "tool.remind_me_search");
        assert_eq!(s["kind"], 1);
        assert_eq!(s["startTimeUnixNano"], "1000000000");
        assert_eq!(s["endTimeUnixNano"], "2000000000");
        assert_eq!(s["status"]["code"], 1);
    }

    #[test]
    fn span_payload_reports_error_status_when_marked() {
        let span = FinishedSpan {
            trace_id: [0u8; 16],
            span_id: [0u8; 8],
            name: "x".to_string(),
            start: UNIX_EPOCH,
            end: UNIX_EPOCH,
            is_error: true,
        };
        let payload = span_payload("svc", &span);
        assert_eq!(
            payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["status"]["code"],
            2
        );
    }

    #[test]
    fn maybe_span_is_a_true_no_op_when_tracing_is_disabled() {
        crate::test_env::remove_var(OTEL_ENABLED_ENV);
        let span = maybe_span("noop.test");
        assert!(span.0.is_none());
        assert!(!is_enabled());
    }
}
