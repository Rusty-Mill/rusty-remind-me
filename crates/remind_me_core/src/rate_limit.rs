//! A fixed-window, in-memory request limiter for the two surfaces that can be
//! exposed over a public tunnel: the webhook ingest endpoint and the remote
//! MCP connector.
//!
//! # Single-process by design, not by omission
//!
//! Each process keeps its own counters entirely in memory. There is no shared
//! store and no distributed coordination, which matches this project's stated
//! non-goals — two nodes behind one tunnel would each enforce the limit
//! separately. Saying that plainly matters more than the limitation itself: an
//! operator who assumes otherwise would under-provision the limit.
//!
//! # No new dependency
//!
//! A map behind a `Mutex`. The critical section does no I/O and never blocks,
//! so it is cheap enough to hold from a worker thread or an async task without
//! becoming a bottleneck — which is what lets the same limiter guard the
//! synchronous webhook server and the async remote endpoint.
//!
//! # Two behaviours worth stating
//!
//! - **A rejected call does not count against the next window.** The stored
//!   count is left untouched on refusal, so a client that backs off and
//!   retries meets a clean window rather than one its own rejected attempts
//!   already primed. Otherwise a client hammering the endpoint could never
//!   recover.
//! - **Stale buckets are pruned lazily**, during an ordinary call, rather than
//!   by a background thread. A long-running server seeing many distinct IPs
//!   would otherwise grow its map without bound, and a cleanup thread would
//!   add a lifecycle to start, stop and join for work this cheap.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const RATE_LIMIT_ENABLED_ENV: &str = "REMIND_ME_RATE_LIMIT_ENABLED";
pub const RATE_LIMIT_REQUESTS_ENV: &str = "REMIND_ME_RATE_LIMIT_REQUESTS";
pub const RATE_LIMIT_WINDOW_ENV: &str = "REMIND_ME_RATE_LIMIT_WINDOW_SECONDS";

pub const DEFAULT_REQUESTS: u32 = 60;
pub const DEFAULT_WINDOW_SECONDS: u64 = 60;

/// Sweep for expired buckets once every this many calls.
const PRUNE_EVERY: u32 = 128;

/// **On by default**, unlike metrics. These two endpoints are reachable from
/// the internet when tunnelled, which is a documented deployment mode, so the
/// safe default is the protective one and the opt-out is explicit.
pub fn rate_limit_enabled() -> bool {
    match std::env::var(RATE_LIMIT_ENABLED_ENV) {
        Ok(raw) => {
            let raw = raw.trim().to_ascii_lowercase();
            !raw.is_empty() && raw != "0" && raw != "false" && raw != "no"
        }
        Err(_) => true,
    }
}

fn configured_u64(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

pub fn configured_requests() -> u32 {
    configured_u64(RATE_LIMIT_REQUESTS_ENV, DEFAULT_REQUESTS as u64) as u32
}

pub fn configured_window() -> Duration {
    Duration::from_secs(configured_u64(
        RATE_LIMIT_WINDOW_ENV,
        DEFAULT_WINDOW_SECONDS,
    ))
}

/// The outcome of one [`RateLimiter::hit`].
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitResult {
    pub allowed: bool,
    /// Seconds until this caller's window resets. Zero when allowed.
    pub retry_after: Duration,
}

/// Round a retry-after up to a whole positive second for the `Retry-After`
/// header, which is defined in whole seconds.
///
/// Never zero: a client that retries immediately on `Retry-After: 0` hits the
/// limiter again before the window has actually rolled over, which looks to it
/// like the backoff is not working.
pub fn retry_after_seconds(retry_after: Duration) -> u64 {
    let secs = retry_after.as_secs_f64().ceil() as u64;
    secs.max(1)
}

pub struct RateLimiter {
    limit: u32,
    window: Duration,
    buckets: Mutex<Buckets>,
}

#[derive(Default)]
struct Buckets {
    /// key → (window start, hits in this window)
    map: HashMap<String, (Instant, u32)>,
    since_prune: u32,
}

impl RateLimiter {
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            buckets: Mutex::new(Buckets::default()),
        }
    }

    /// Record one hit against `key`.
    ///
    /// `now` is taken as a parameter so tests can drive the window
    /// deterministically instead of sleeping — the same clock-injection
    /// convention this crate already uses for vitality.
    pub fn hit_at(&self, key: &str, now: Instant) -> RateLimitResult {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        self.maybe_prune(&mut buckets, now);

        let (window_start, count) = buckets.map.get(key).copied().unwrap_or((now, 0));

        // `saturating_duration_since`, not subtraction: `Instant` arithmetic
        // panics on a negative delta, and a caller passing a slightly earlier
        // `now` must not take the endpoint down.
        let elapsed = now.saturating_duration_since(window_start);
        let (window_start, count) = if elapsed >= self.window {
            (now, 0)
        } else {
            (window_start, count)
        };

        if count >= self.limit {
            // Left unchanged, deliberately: a rejected call must not extend
            // the window it was rejected by, or a client that keeps retrying
            // can never get back in.
            buckets.map.insert(key.to_string(), (window_start, count));
            let retry_after = self
                .window
                .saturating_sub(now.saturating_duration_since(window_start));
            drop(buckets);
            crate::metrics::record_rate_limit_rejection();
            return RateLimitResult {
                allowed: false,
                retry_after,
            };
        }

        buckets
            .map
            .insert(key.to_string(), (window_start, count + 1));
        RateLimitResult {
            allowed: true,
            retry_after: Duration::ZERO,
        }
    }

    pub fn hit(&self, key: &str) -> RateLimitResult {
        self.hit_at(key, Instant::now())
    }

    fn maybe_prune(&self, buckets: &mut Buckets, now: Instant) {
        buckets.since_prune += 1;
        if buckets.since_prune < PRUNE_EVERY {
            return;
        }
        buckets.since_prune = 0;
        let window = self.window;
        buckets
            .map
            .retain(|_, (start, _)| now.saturating_duration_since(*start) < window);
    }

    pub fn reset(&self) {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        buckets.map.clear();
        buckets.since_prune = 0;
    }

    pub fn tracked_keys(&self) -> usize {
        self.buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }
}

/// The process-wide limiter both guarded surfaces share.
///
/// One limiter rather than one per endpoint: the limit is a property of the
/// caller, and a client turned away by the webhook should not get a fresh
/// allowance by switching to the MCP endpoint on the same host.
pub fn shared() -> &'static RateLimiter {
    static SHARED: OnceLock<RateLimiter> = OnceLock::new();
    SHARED.get_or_init(|| RateLimiter::new(configured_requests(), configured_window()))
}

/// Which bucket a request counts against.
///
/// A caller presenting the correct secret gets its own shared `auth:known`
/// bucket rather than a per-IP one. That is deliberate: the legitimate
/// integration is *identified*, so it is limited as one client no matter how
/// many addresses it dials from, while every unauthenticated caller is
/// isolated to its own IP bucket and cannot exhaust anyone else's allowance.
///
/// Compared in constant time — this runs before authentication on a
/// public-facing endpoint, so a fast-path `==` would leak the secret to a
/// timing probe that never needed to authenticate at all.
pub fn resolve_key(presented: &str, remote_addr: &str, known_secret: Option<&str>) -> String {
    if let Some(secret) = known_secret {
        if !secret.is_empty()
            && !presented.is_empty()
            && crate::webhook::constant_time_eq(presented.as_bytes(), secret.as_bytes())
        {
            return "auth:known".to_string();
        }
    }
    format!(
        "ip:{}",
        if remote_addr.is_empty() {
            "unknown"
        } else {
            remote_addr
        }
    )
}

/// Check the shared limiter, or `None` when limiting is disabled.
///
/// Returns `Some(result)` so a caller can distinguish "allowed" from "not
/// enforced" — they are the same for the request but not for a status report.
pub fn check(key: &str) -> Option<RateLimitResult> {
    if !rate_limit_enabled() {
        return None;
    }
    Some(shared().hit(key))
}
