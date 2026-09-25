//! Outbound notification channels.
//!
//! # Availability is configuration, not a switch
//!
//! There is no separate enable flag. A webhook URL being set *is* the opt-in,
//! the same way this crate decides embedder and reranker availability. That is
//! what lets every caller — the reminder scheduler's delivery hook, and later
//! anything else with something to say — call [`notify`] unconditionally
//! without first asking whether anything is listening.
//!
//! # One generic sink, no per-service formatting
//!
//! A single `REMIND_ME_NOTIFY_WEBHOOK_URL` covers ntfy, Slack, Discord,
//! Mattermost and Pushover-via-webhook uniformly, with the payload always
//! `{"subject", "body", "source": "remind-me"}`. Native formatting on any one
//! of those (Slack blocks, ntfy priority headers) needs a small relay in front
//! — a known limitation rather than N service-specific formatters to build and
//! keep working.
//!
//! # Known limitation: `http://` only
//!
//! This crate's HTTP client is a hand-rolled `TcpStream` one with no TLS, a
//! choice `sync::http` and `embedder` both already made — a deployment that
//! needs TLS puts a reverse proxy in front. That is a much sharper constraint
//! here than it is for sync: a sync peer is usually on your own network, but
//! the webhook endpoints people actually use are public HTTPS. Reaching them
//! directly needs a TLS-capable client, which is a dependency decision rather
//! than an implementation detail, so it is deliberately left open — see
//! `docs/parity-loop-decisions.md`. The reference gets TLS free from `httpx`;
//! it also gets SMTP free from `smtplib`, which is why it has an email channel
//! and this does not.
//!
//! # A channel that fails never propagates
//!
//! [`notify`] returns how many channels accepted the message and never fails.
//! Its callers are background loops delivering something else; a dead webhook
//! must not take the loop down with it, and one broken channel must not stop
//! another from being tried.

use std::time::Duration;

pub const WEBHOOK_URL_ENV: &str = "REMIND_ME_NOTIFY_WEBHOOK_URL";
pub const WEBHOOK_TIMEOUT_ENV: &str = "REMIND_ME_NOTIFY_WEBHOOK_TIMEOUT";

/// Seconds before a webhook POST is abandoned. Short on purpose: callers are
/// synchronous background loops, and a hung endpoint must not stall a poll
/// pass indefinitely.
pub const DEFAULT_WEBHOOK_TIMEOUT_SECONDS: u64 = 5;

pub fn configured_webhook_url() -> String {
    std::env::var(WEBHOOK_URL_ENV).unwrap_or_default()
}

pub fn configured_webhook_timeout() -> Duration {
    Duration::from_secs(
        std::env::var(WEBHOOK_TIMEOUT_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .unwrap_or(DEFAULT_WEBHOOK_TIMEOUT_SECONDS),
    )
}

/// One outbound channel.
pub trait Notifier {
    /// Stable name, used when reporting that this channel failed.
    fn name(&self) -> &'static str;
    /// Attempt delivery. Returns whether it succeeded, and never panics.
    fn send(&self, subject: &str, body: &str) -> bool;
}

/// POSTs a generic JSON payload to a configured webhook URL.
pub struct WebhookNotifier {
    pub url: String,
}

/// The body a webhook receives. Named rather than built inline so the wire
/// shape is one greppable thing — a receiver's parser is written against it.
pub fn webhook_payload(subject: &str, body: &str) -> serde_json::Value {
    serde_json::json!({
        "subject": subject,
        "body": body,
        "source": "remind-me",
    })
}

impl Notifier for WebhookNotifier {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn send(&self, subject: &str, body: &str) -> bool {
        let payload = webhook_payload(subject, body).to_string();
        match crate::sync::http::post_json_unauthenticated(&self.url, &payload) {
            // Any 2xx counts. A receiver that answers 204 has accepted it just
            // as much as one that answers 200 with a body.
            Ok((status, _)) if (200..300).contains(&status) => true,
            Ok((status, _)) => {
                eprintln!(
                    "notify: webhook {} returned {} — notification not delivered",
                    self.url, status
                );
                false
            }
            Err(e) => {
                eprintln!("notify: webhook {} failed: {}", self.url, e);
                false
            }
        }
    }
}

/// Whichever channels are configured — possibly none.
pub fn configured_notifiers() -> Vec<Box<dyn Notifier>> {
    let mut notifiers: Vec<Box<dyn Notifier>> = Vec::new();
    let url = configured_webhook_url();
    if !url.trim().is_empty() {
        notifiers.push(Box::new(WebhookNotifier {
            url: url.trim().to_string(),
        }));
    }
    notifiers
}

/// Whether any channel is configured, without building one.
pub fn any_channel_configured() -> bool {
    !configured_webhook_url().trim().is_empty()
}

/// Fan out to every configured channel. Returns how many accepted it.
///
/// A no-op returning 0 when nothing is configured, so a caller never has to
/// check availability first.
pub fn notify(subject: &str, body: &str) -> usize {
    configured_notifiers()
        .iter()
        .filter(|notifier| {
            let ok = notifier.send(subject, body);
            if !ok {
                eprintln!("notify: channel '{}' did not accept", notifier.name());
            }
            ok
        })
        .count()
}
