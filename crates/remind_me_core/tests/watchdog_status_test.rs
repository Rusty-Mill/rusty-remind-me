//! The watchdog reaches `server_status` (issue #168).
//!
//! The unit tests in `watchdog.rs` cover the watchdog's own behaviour against
//! locally-constructed instances. This covers the wiring: that
//! `status::server_status` actually reports the *process-wide* watchdog, which
//! is what `remind_me_server_status` serializes.

use remind_me_core::{status, watchdog, Database};

#[test]
fn server_status_reports_the_watchdog() {
    let db = Database::open(":memory:").expect("in-memory database");
    let report = status::server_status(&db.conn()).expect("status snapshot");

    // Unset `REMIND_ME_SLOW_CALL_SECONDS` means the 30s default, so the
    // watchdog is enabled in an ordinary test process.
    assert!(
        report.watchdog.enabled,
        "the watchdog defaults to enabled, matching the reference"
    );
    assert_eq!(
        report.watchdog.threshold_seconds,
        Some(watchdog::DEFAULT_SLOW_CALL_SECONDS)
    );
}

#[test]
fn an_in_flight_call_shows_up_in_server_status() {
    let db = Database::open(":memory:").expect("in-memory database");

    let before = status::server_status(&db.conn())
        .expect("status snapshot")
        .watchdog
        .calls_in_flight;

    let guard = watchdog::arm("remind_me_search");
    let during = status::server_status(&db.conn())
        .expect("status snapshot")
        .watchdog
        .calls_in_flight;
    assert_eq!(
        during,
        before + 1,
        "an armed call should be visible in the status report"
    );

    drop(guard);
    let after = status::server_status(&db.conn())
        .expect("status snapshot")
        .watchdog
        .calls_in_flight;
    assert_eq!(after, before, "dropping the guard should disarm the call");
}

#[test]
fn the_status_payload_serializes_with_the_watchdog_field() {
    let db = Database::open(":memory:").expect("in-memory database");
    let report = status::server_status(&db.conn()).expect("status snapshot");
    let payload = serde_json::to_value(&report).expect("ServerStatus should serialize");

    // This is the shape `remind_me_server_status` hands a caller, so the field
    // names are part of the contract, not an implementation detail.
    assert!(
        payload.get("watchdog").is_some(),
        "server_status JSON must carry a watchdog object, got: {}",
        payload
    );
    assert!(payload["watchdog"].get("enabled").is_some());
    assert!(payload["watchdog"].get("threshold_seconds").is_some());
    assert!(payload["watchdog"].get("calls_in_flight").is_some());
}
