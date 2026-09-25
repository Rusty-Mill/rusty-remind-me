//! `telemetry::maybe_span`'s background exporter actually reaches a
//! collector, end to end -- a real `TcpListener` standing in for one.
//!
//! `telemetry`'s exporter thread is a process-wide, initialize-once
//! singleton (matching the reference's own `_tracer`/`_init_attempted`
//! module globals, which only ever build a tracer once per process too) --
//! so this crate's convention of one integration-test file per OS process
//! is load-bearing here, not just style: every test in this file shares the
//! same enabled/endpoint configuration, and no other file may also enable
//! tracing without its own separate process.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::telemetry::{self, OTEL_ENABLED_ENV, OTEL_ENDPOINT_ENV};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::channel;
use std::time::Duration;

#[test]
fn a_dropped_span_reaches_a_real_collector_with_the_right_json_shape() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = channel();
    let handle = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(response);
            let _ = tx.send(body);
        }
    });

    crate::test_env::set_var(OTEL_ENABLED_ENV, "1");
    crate::test_env::set_var(
        OTEL_ENDPOINT_ENV,
        format!("http://127.0.0.1:{port}/v1/traces"),
    );

    {
        let _span = telemetry::maybe_span("tool.remind_me_search");
    }

    let body = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the exporter thread should have POSTed a span within 5s");
    handle.join().unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("body must be valid JSON");
    let span = &parsed["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    assert_eq!(span["name"], "tool.remind_me_search");
    assert_eq!(
        span["status"]["code"], 1,
        "a clean span reports OK, not UNSET or ERROR"
    );
    assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
    assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
    assert!(telemetry::is_enabled());
    assert!(
        telemetry::last_error().is_none(),
        "a successful export must not report a latch reason"
    );
}
