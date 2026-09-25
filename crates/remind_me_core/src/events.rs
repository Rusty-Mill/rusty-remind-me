//! Raw automation event stream for memory mutations.
//!
//! # Separate from notifications on purpose, not by accident
//!
//! Both POST JSON to a configured webhook, and they are still two different
//! things:
//!
//! - [`crate::notifications`] is **human-facing and throttled** — a fired
//!   reminder, a faulted sync verdict — meant to be read by a person, with
//!   deliberate suppression of repeats so a persistent fault does not become
//!   alert fatigue.
//! - This is **automation-facing and never throttled** — a relay, a second
//!   indexer, an audit log. Completeness is the whole point: suppressing a
//!   "repeat" event would silently drop a real mutation the consumer needs to
//!   see.
//!
//! Same transport, opposite requirements. Reusing the notify path would give
//! automation consumers the throttle, which breaks them quietly.
//!
//! # Metadata only — never memory content
//!
//! The payload is `event`, `memory_id`, `category`, `timestamp`. Nothing else,
//! and specifically not content, tags or metadata.
//!
//! This is an event *notification* stream, not a content-sync mechanism. A
//! consumer that wants the memory calls back through the API or the MCP tools
//! with the id, at which point the ordinary sensitive-memory rules apply. Put
//! content on the wire here and every configured webhook silently becomes an
//! egress path for the entire vault, sensitive memories included, with no
//! per-call intent to check against.
//!
//! # Sync-applied writes emit nothing
//!
//! Only local mutations emit. A record arriving from a peer is not a local
//! event, and emitting it is how two synced nodes would echo each other's
//! mutations back and forth forever.
//!
//! # Emission never delays or fails the write that caused it
//!
//! The POST happens on a detached thread whose handle is deliberately held
//! until it finishes. A write must not wait on a webhook, and a webhook that
//! is down must not fail the write — but a fire-and-forget handle that is
//! dropped immediately can also lose the POST mid-flight, which is the
//! failure this guards against.

use std::sync::Mutex;
use std::thread::JoinHandle;

/// Where automation events are POSTed. Distinct from the notify URL.
pub const EVENT_WEBHOOK_URL_ENV: &str = "REMIND_ME_EVENT_WEBHOOK_URL";

/// A memory mutation worth telling automation about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Created,
    Updated,
    Deleted,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::Created => "created",
            Event::Updated => "updated",
            Event::Deleted => "deleted",
        }
    }
}

pub fn configured_url() -> String {
    std::env::var(EVENT_WEBHOOK_URL_ENV).unwrap_or_default()
}

/// Whether an automation consumer is configured at all.
pub fn enabled() -> bool {
    !configured_url().trim().is_empty()
}

/// The wire shape, named rather than built inline so a consumer's parser is
/// written against one greppable thing.
///
/// Deliberately carries no memory content — see the module docs.
pub fn payload(event: Event, memory_id: &str, category: &str) -> serde_json::Value {
    serde_json::json!({
        "event": event.as_str(),
        "memory_id": memory_id,
        "category": category,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    })
}

/// In-flight POSTs, held so a detached thread cannot be lost mid-request.
fn in_flight() -> &'static Mutex<Vec<JoinHandle<()>>> {
    static IN_FLIGHT: std::sync::OnceLock<Mutex<Vec<JoinHandle<()>>>> = std::sync::OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(Vec::new()))
}

/// Emit one mutation event.
///
/// A true no-op when unconfigured: nothing is built and no thread is started,
/// rather than spawning work that discovers it has nowhere to go.
///
/// Never blocks and never fails the caller. A write is the user's data; a
/// webhook is someone's convenience, and the second must not be able to cost
/// the first.
pub fn emit(event: Event, memory_id: &str, category: &str) {
    let url = configured_url();
    if url.trim().is_empty() {
        return;
    }

    let body = payload(event, memory_id, category).to_string();
    let target = url.clone();
    let handle = std::thread::spawn(move || {
        match crate::sync::http::post_json_unauthenticated(&target, &body) {
            // Any 2xx: a receiver answering 204 has accepted it as much as one
            // answering 200 with a body.
            Ok((status, _)) if (200..300).contains(&status) => {}
            Ok((status, _)) => {
                eprintln!("events: webhook {} returned {}", target, status);
            }
            Err(e) => {
                eprintln!("events: webhook {} failed: {}", target, e);
            }
        }
    });

    let mut guard = in_flight().lock().unwrap_or_else(|e| e.into_inner());
    // Reap finished threads while we are here, so the list cannot grow without
    // bound on a busy vault.
    guard.retain(|h| !h.is_finished());
    guard.push(handle);
}

/// Wait for every in-flight POST to finish.
///
/// For tests, and for a clean shutdown: a process that exits while a POST is
/// still in flight drops it, which is the same silent loss the held handle
/// exists to prevent.
pub fn drain() {
    let handles: Vec<JoinHandle<()>> = {
        let mut guard = in_flight().lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    };
    for handle in handles {
        let _ = handle.join();
    }
}
