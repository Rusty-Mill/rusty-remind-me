//! Coverage for the request rate limiter (gap E2, issue #121).
//!
//! The window is driven by an injected clock rather than by sleeping: a test
//! that sleeps a real 60 seconds gets deleted the first time someone is in a
//! hurry, and one that sleeps a *shortened* window is a race waiting to fail
//! on a loaded CI box.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::rate_limit::{
    rate_limit_enabled, resolve_key, retry_after_seconds, RateLimiter, DEFAULT_REQUESTS,
    DEFAULT_WINDOW_SECONDS, RATE_LIMIT_ENABLED_ENV,
};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn limiter(limit: u32) -> RateLimiter {
    RateLimiter::new(limit, Duration::from_secs(60))
}

#[test]
fn requests_under_the_limit_are_allowed() {
    let limiter = limiter(3);
    let now = Instant::now();

    for i in 0..3 {
        assert!(
            limiter.hit_at("ip:1.2.3.4", now).allowed,
            "request {i} refused while under the limit"
        );
    }
}

#[test]
fn the_request_past_the_limit_is_refused_with_a_retry_after() {
    let limiter = limiter(2);
    let now = Instant::now();
    limiter.hit_at("ip:1.2.3.4", now);
    limiter.hit_at("ip:1.2.3.4", now);

    let verdict = limiter.hit_at("ip:1.2.3.4", now);

    assert!(!verdict.allowed);
    // A refusal with no indication of when to come back invites an immediate
    // retry, which is the behaviour the limiter exists to stop.
    assert!(verdict.retry_after > Duration::ZERO);
}

#[test]
fn a_refused_request_does_not_extend_the_window_it_was_refused_by() {
    let limiter = limiter(1);
    let start = Instant::now();
    assert!(limiter.hit_at("ip:1.2.3.4", start).allowed);

    // Hammer while refused. Each of these must leave the stored window
    // untouched — counting them would push the reset further out every time
    // and a client that kept retrying could never get back in.
    for offset in [1, 2, 3, 10, 30] {
        let verdict = limiter.hit_at("ip:1.2.3.4", start + Duration::from_secs(offset));
        assert!(!verdict.allowed);
    }

    // The window still ends 60s after the *first* request, not after the last
    // rejected one.
    assert!(
        limiter
            .hit_at("ip:1.2.3.4", start + Duration::from_secs(60))
            .allowed,
        "the window was pushed out by rejected attempts"
    );
}

#[test]
fn the_window_expires_and_the_allowance_returns() {
    let limiter = limiter(2);
    let start = Instant::now();
    limiter.hit_at("ip:1.2.3.4", start);
    limiter.hit_at("ip:1.2.3.4", start);
    assert!(!limiter.hit_at("ip:1.2.3.4", start).allowed);

    assert!(
        limiter
            .hit_at("ip:1.2.3.4", start + Duration::from_secs(60))
            .allowed
    );
}

#[test]
fn the_window_boundary_is_inclusive_of_the_reset() {
    let limiter = limiter(1);
    let start = Instant::now();
    limiter.hit_at("k", start);

    // Pinned in both directions so a later `>` cannot quietly hold a client
    // out for one extra second, nor a `>=` let them in early.
    assert!(!limiter.hit_at("k", start + Duration::from_secs(59)).allowed);
    assert!(limiter.hit_at("k", start + Duration::from_secs(60)).allowed);
}

#[test]
fn one_callers_flood_does_not_consume_anothers_allowance() {
    let limiter = limiter(1);
    let now = Instant::now();
    limiter.hit_at("ip:1.1.1.1", now);
    assert!(!limiter.hit_at("ip:1.1.1.1", now).allowed);

    // Buckets are per key. Otherwise one abusive address is a denial of
    // service against every other caller, which is worse than no limiter.
    assert!(limiter.hit_at("ip:2.2.2.2", now).allowed);
}

#[test]
fn concurrent_callers_are_counted_exactly_once_each() {
    use std::sync::Arc;

    let limiter = Arc::new(RateLimiter::new(50, Duration::from_secs(60)));
    let mut handles = Vec::new();

    // 8 threads × 25 requests = 200 against a limit of 50. Exactly 50 must be
    // allowed: a lost update would let more through, and double-counting
    // fewer. Either way the limit stops meaning what it says.
    for _ in 0..8 {
        let limiter = Arc::clone(&limiter);
        handles.push(std::thread::spawn(move || {
            let now = Instant::now();
            (0..25)
                .filter(|_| limiter.hit_at("shared", now).allowed)
                .count()
        }));
    }

    let allowed: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(allowed, 50, "concurrent hits were miscounted");
}

#[test]
fn expired_buckets_are_pruned_rather_than_accumulating() {
    let limiter = RateLimiter::new(10, Duration::from_secs(60));
    let start = Instant::now();

    // A long-running server sees many distinct addresses. Without pruning
    // the map grows for the life of the process, which is a slow leak on
    // exactly the endpoint most exposed to strangers.
    for i in 0..200 {
        limiter.hit_at(&format!("ip:10.0.0.{i}"), start);
    }
    let before = limiter.tracked_keys();

    for i in 0..200 {
        limiter.hit_at(
            &format!("ip:10.0.1.{i}"),
            start + Duration::from_secs(3_600),
        );
    }

    assert!(
        limiter.tracked_keys() < before + 200,
        "expired buckets accumulated: {} keys",
        limiter.tracked_keys()
    );
}

// ---------------------------------------------------------------------------
// Bucket keys
// ---------------------------------------------------------------------------

#[test]
fn the_right_secret_shares_one_authenticated_bucket() {
    // A legitimate integration is identified, so it is limited as one client
    // however many addresses it dials from.
    assert_eq!(
        resolve_key("s3cret", "1.1.1.1", Some("s3cret")),
        "auth:known"
    );
    assert_eq!(
        resolve_key("s3cret", "2.2.2.2", Some("s3cret")),
        "auth:known"
    );
}

#[test]
fn a_wrong_or_absent_secret_falls_back_to_the_callers_address() {
    // Every unauthenticated caller is isolated to its own bucket and cannot
    // exhaust anyone else's allowance.
    assert_eq!(
        resolve_key("wrong", "1.1.1.1", Some("s3cret")),
        "ip:1.1.1.1"
    );
    assert_eq!(resolve_key("", "1.1.1.1", Some("s3cret")), "ip:1.1.1.1");
    assert_eq!(resolve_key("anything", "1.1.1.1", None), "ip:1.1.1.1");
}

#[test]
fn an_unknown_address_shares_a_bucket_rather_than_bypassing_the_limit() {
    // An unidentifiable caller is precisely the one not to exempt.
    assert_eq!(resolve_key("", "", Some("s3cret")), "ip:unknown");
}

#[test]
fn an_empty_configured_secret_never_matches() {
    // A deployment with no secret set must not have every caller collapse
    // into the shared authenticated bucket, which would make the limit
    // global and trivially exhaustible by one stranger.
    assert_eq!(resolve_key("", "1.1.1.1", Some("")), "ip:1.1.1.1");
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[test]
fn limiting_is_on_by_default() {
    let _guard = env_lock().lock().unwrap();
    crate::test_env::remove_var(RATE_LIMIT_ENABLED_ENV);

    // Unlike metrics, the safe default here is the protective one: both
    // guarded surfaces are reachable from the internet when tunnelled.
    assert!(rate_limit_enabled());
    assert_eq!(DEFAULT_REQUESTS, 60);
    assert_eq!(DEFAULT_WINDOW_SECONDS, 60);
}

#[test]
fn an_empty_string_is_the_explicit_opt_out() {
    let _guard = env_lock().lock().unwrap();
    crate::test_env::set_var(RATE_LIMIT_ENABLED_ENV, "");
    assert!(!rate_limit_enabled());
    crate::test_env::set_var(RATE_LIMIT_ENABLED_ENV, "0");
    assert!(!rate_limit_enabled());
    crate::test_env::set_var(RATE_LIMIT_ENABLED_ENV, "1");
    assert!(rate_limit_enabled());
    crate::test_env::remove_var(RATE_LIMIT_ENABLED_ENV);
}

#[test]
fn retry_after_is_always_a_whole_positive_second() {
    // `Retry-After: 0` invites an immediate retry into the same wall, which
    // looks to the client like the backoff is broken.
    assert_eq!(retry_after_seconds(Duration::ZERO), 1);
    assert_eq!(retry_after_seconds(Duration::from_millis(1)), 1);
    assert_eq!(retry_after_seconds(Duration::from_millis(1_500)), 2);
    assert_eq!(retry_after_seconds(Duration::from_secs(30)), 30);
}
