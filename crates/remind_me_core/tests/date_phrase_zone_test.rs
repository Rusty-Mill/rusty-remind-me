//! `REMIND_ME_TZ` end to end. In its own binary: it writes the process env.

#[path = "../src/test_env.rs"]
mod test_env;

use chrono::{DateTime, Utc};
use remind_me_core::context_brief::parse_since;
use remind_me_core::date_phrase::{configured_zone, TZ_ENV};
use remind_me_core::time_window::{RawTimeWindow, TimeField, TimeWindow};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Sets `REMIND_ME_TZ` for one test and restores what was there.
struct Zone(Option<String>);

impl Zone {
    fn set(value: Option<&str>) -> Self {
        let old = std::env::var(TZ_ENV).ok();
        match value {
            Some(v) => test_env::set_var(TZ_ENV, v),
            None => test_env::remove_var(TZ_ENV),
        }
        Self(old)
    }
}

impl Drop for Zone {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => test_env::set_var(TZ_ENV, v),
            None => test_env::remove_var(TZ_ENV),
        }
    }
}

fn utc(at: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(at)
        .unwrap()
        .with_timezone(&Utc)
}

fn since(value: &str) -> RawTimeWindow {
    RawTimeWindow {
        since: Some(value.into()),
        until: None,
        time_field: TimeField::Created,
    }
}

#[test]
fn unset_or_blank_is_utc_and_a_bad_name_is_an_error() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _zone = Zone::set(None);
    assert_eq!(configured_zone(), Ok(chrono_tz::Tz::UTC));
    let _zone = Zone::set(Some("  "));
    assert_eq!(configured_zone(), Ok(chrono_tz::Tz::UTC));
    let _zone = Zone::set(Some(" Australia/Sydney "));
    assert_eq!(configured_zone(), Ok(chrono_tz::Australia::Sydney));
    let _zone = Zone::set(Some("AEDT"));
    assert!(configured_zone().unwrap_err().contains("AEDT"));
}

#[test]
fn phrases_read_the_configured_zone() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let now = utc("2026-10-27T22:00:00Z"); // Wednesday 09:00 in Sydney
    let _zone = Zone::set(Some("Australia/Sydney"));
    let w = TimeWindow::resolve(&since("yesterday"), now).unwrap();
    let back = RawTimeWindow::from(w);
    assert_eq!(back.since.as_deref(), Some("2026-10-26T13:00:00+00:00"));
    assert_eq!(back.until.as_deref(), Some("2026-10-27T13:00:00+00:00"));
    assert_eq!(
        parse_since("today", now).unwrap(),
        utc("2026-10-27T13:00:00Z")
    );

    let _zone = Zone::set(None);
    let back = RawTimeWindow::from(TimeWindow::resolve(&since("yesterday"), now).unwrap());
    assert_eq!(back.since.as_deref(), Some("2026-10-26T00:00:00+00:00"));
}

#[test]
fn a_bad_zone_fails_phrases_but_never_fixed_bounds() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let now = utc("2026-10-27T22:00:00Z");
    let _zone = Zone::set(Some("Mars/Olympus"));
    let err = TimeWindow::resolve(&since("yesterday"), now).unwrap_err();
    assert!(
        err.contains("`since`") && err.contains("Mars/Olympus"),
        "{err}"
    );
    assert!(parse_since("today", now)
        .unwrap_err()
        .to_string()
        .contains("Mars/Olympus"));
    assert!(TimeWindow::resolve(&since("2d"), now).is_ok());
    assert!(TimeWindow::resolve(&since("2026-10-20T00:00:00Z"), now).is_ok());
    assert!(parse_since("7d", now).is_ok());
}
