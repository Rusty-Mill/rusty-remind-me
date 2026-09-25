//! A collector that can't be reached permanently latches tracing off for
//! the rest of the run -- matching the reference's `_get_tracer()` disabling
//! itself on any failure. Its own single-test-per-process file, for the
//! same reason `telemetry_export_test.rs` is: the exporter is a process-wide,
//! initialize-once singleton.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::telemetry::{self, OTEL_ENABLED_ENV, OTEL_ENDPOINT_ENV};
use std::net::TcpListener;
use std::time::Duration;

#[test]
fn an_unreachable_collector_permanently_disables_tracing_after_one_failure() {
    // Bind then drop: a real, momentarily-valid port nothing is listening on.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    crate::test_env::set_var(OTEL_ENABLED_ENV, "1");
    crate::test_env::set_var(
        OTEL_ENDPOINT_ENV,
        format!("http://127.0.0.1:{port}/v1/traces"),
    );

    assert!(
        telemetry::is_enabled(),
        "tracing starts enabled -- the failure hasn't happened yet"
    );

    {
        let _span = telemetry::maybe_span("tool.will_fail");
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while telemetry::last_error().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        telemetry::last_error().is_some(),
        "an unreachable collector must record why the exporter gave up"
    );
    assert!(
        !telemetry::is_enabled(),
        "tracing must latch off for the rest of the run, not just this one failed export"
    );

    // The latch is permanent: a span opened after the failure is a no-op,
    // proven by nothing panicking and is_enabled staying false.
    {
        let _span = telemetry::maybe_span("tool.after_the_latch");
    }
    assert!(!telemetry::is_enabled());
}
