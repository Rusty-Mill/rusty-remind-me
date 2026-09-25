//! The remind-me sync hub: a central sync point speaking the peer protocol.
//!
//! A port of the reference's `hub/main.py` — a FastAPI server backed by
//! Postgres — with one deliberate addition: the storage backend is behind a
//! trait, so the same hub runs on Postgres (the drop-in for an existing
//! deployment) or on SQLite (a self-hosted hub that wants one file and no
//! server). `docs/adr/0015` records why. A third backend, the embedded
//! `rusty_multimodal_db` engine behind the `multimodal-store` feature, is
//! the one `docs/adr/0021` plans to replace both with.
//!
//! # What a hub is, relative to a peer
//!
//! The same wire protocol against a different topology. Nodes push to and pull
//! from a hub; the hub never pulls. `remind_me_core::sync::server` already
//! serves the peer side of this protocol, and a node cannot tell the two
//! apart — which is the point.
//!
//! Routes:
//!
//! | Route | Auth | Purpose |
//! | --- | --- | --- |
//! | `GET /health` | none | liveness; 200 reachable, 503 not |
//! | `GET /stats` | bearer | the full aggregate, once per reconcile |
//! | `GET /count` | bearer | scalar counts, cheap enough to poll |
//! | `GET /metrics` | bearer | Prometheus text, off by default |
//! | `POST /admin/compact_tombstones` | bearer | hard-delete expired tombstones |
//! | `POST /sync/push` | bearer | upsert a batch, LWW on `updated_at` |
//! | `GET /sync/pull` | bearer | memory records since a cursor |
//! | `GET /sync/pull_entities` | bearer | entity records |
//! | `GET /sync/pull_links` | bearer | memory↔entity links |
//! | `GET /sync/pull_entity_relations` | bearer | typed entity edges |
//!
//! # The one hub-only column
//!
//! `origin_node` records *which node pushed* a record, and never leaves the
//! hub. Pull's `exclude_node` filters on it rather than on the record's own
//! `node_id`, and that difference is deliberate: a client never rewrites
//! `node_id` on update, so filtering on it would make a record's creator deaf
//! to every later edit other nodes push. Peers compensate by pushing to each
//! other; a hub is pull-only, so it must track pushers itself.

pub mod canon;
pub mod http;
pub mod record;
pub mod routes;
pub mod store;

use http::{Head, Response};
use store::HubStore;

/// Version of the hub server, reported by `/health`, `/count`, `/stats`,
/// `/metrics` and the `X-Hub-Version` header on every response.
///
/// Versioned independently of the crate: the hub is a separate deployable on
/// its own release cadence, and tying it to the workspace version would mean
/// the reported version churned on releases that never touched hub code —
/// worse than useless for the question it answers, which is "does the hub I am
/// talking to have the endpoint I need?".
///
/// Matches the reference's `HUB_VERSION` at the time of the port. Bump it
/// (semver) whenever observable behaviour changes: MAJOR for a wire break,
/// MINOR for a new endpoint or response field, PATCH for a fix nothing can key
/// off. Clients needing to know whether a capability exists should still probe
/// for the 404 rather than compare versions — this is a diagnostic, not a
/// feature-negotiation channel.
pub const HUB_VERSION: &str = "1.6.0";

/// The cursor a client sends when it has never synced.
pub const EPOCH: &str = "1970-01-01T00:00:00+00:00";

/// Runtime configuration, resolved once at startup.
#[derive(Debug, Clone)]
pub struct Config {
    /// Shared bearer secret. Empty means *reject everything*, never
    /// *allow everything* — see [`authorized`].
    pub secret: String,
    pub metrics_enabled: bool,
    pub tombstone_retention_days: i64,
}

impl Config {
    /// Read configuration from the environment, matching the reference's names.
    pub fn from_env() -> Self {
        Self {
            secret: std::env::var("SYNC_SECRET").unwrap_or_default(),
            metrics_enabled: matches!(
                std::env::var("REMIND_ME_HUB_METRICS_ENABLED")
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "1" | "true" | "yes"
            ),
            tombstone_retention_days: std::env::var("REMIND_ME_HUB_TOMBSTONE_RETENTION_DAYS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(90),
        }
    }
}

/// Constant-time bearer check that always rejects when no secret is set.
///
/// Two properties, both load-bearing:
///
/// - **No secret means no access.** An unconfigured hub refusing everything is
///   the only safe reading; the alternative — an empty secret matching an
///   empty header — is an open door that looks like configuration.
/// - **Comparison is constant-time over bytes.** Early-exit comparison leaks
///   the secret one byte at a time to anyone who can time a request, and this
///   is documented as commonly reachable through a tunnel from the open
///   internet.
pub fn authorized(config: &Config, header: &str) -> bool {
    if config.secret.is_empty() {
        return false;
    }
    let expected = format!("Bearer {}", config.secret);
    constant_time_eq(header.as_bytes(), expected.as_bytes())
}

/// Length-independent, data-independent byte comparison.
///
/// The length check is intentionally *not* an early return on mismatch: it
/// folds into the same accumulator, so a wrong-length header costs the same
/// time as a wrong-value one.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

/// Whether a route requires the bearer secret.
///
/// `/health` is the sole exception, and deliberately: it is what a deploy
/// healthcheck polls, and it must keep answering when the database is down.
fn requires_auth(path: &str) -> bool {
    path != "/health"
}

/// Route one parsed request to its handler.
///
/// Split from the socket loop so the entire surface is testable without a
/// listener, which is what lets the route tests run against a SQLite store
/// in-process.
pub fn dispatch(store: &dyn HubStore, config: &Config, head: &Head, body: &[u8]) -> Response {
    if requires_auth(&head.path) && !authorized(config, &head.authorization) {
        return Response::error(401, "unauthorized");
    }

    let get = head.method == "GET";
    let post = head.method == "POST";

    match head.path.as_str() {
        "/health" if get => routes::health(store),
        "/stats" if get => routes::stats(store),
        "/count" if get => routes::count(store, &head.query),
        "/metrics" if get => routes::metrics(store, config),
        "/admin/compact_tombstones" if post => routes::compact_tombstones(store, config),
        "/sync/push" if post => routes::push(store, body),
        "/sync/pull" if get => routes::pull(store, &head.query),
        "/sync/pull_entities" if get => routes::pull_entities(store, &head.query),
        "/sync/pull_links" if get => routes::pull_links(store, &head.query),
        "/sync/pull_entity_relations" if get => routes::pull_entity_relations(store, &head.query),
        // A known path with the wrong method is 405, not 404: the distinction
        // is what tells a client "you have the wrong verb" rather than "this
        // hub is too old to have that endpoint", and clients probe for the
        // 404 to detect capabilities.
        "/health"
        | "/stats"
        | "/count"
        | "/metrics"
        | "/admin/compact_tombstones"
        | "/sync/push"
        | "/sync/pull"
        | "/sync/pull_entities"
        | "/sync/pull_links"
        | "/sync/pull_entity_relations" => Response::error(405, "method not allowed"),
        _ => Response::error(404, "not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(secret: &str) -> Config {
        Config {
            secret: secret.to_string(),
            metrics_enabled: false,
            tombstone_retention_days: 90,
        }
    }

    #[test]
    fn an_unconfigured_hub_rejects_everything_including_an_empty_header() {
        // The failure mode this guards is an empty secret matching an empty
        // header, which would be an open door that looks like configuration.
        let c = config("");
        assert!(!authorized(&c, ""));
        assert!(!authorized(&c, "Bearer "));
        assert!(!authorized(&c, "Bearer anything"));
    }

    #[test]
    fn the_correct_bearer_is_accepted_and_near_misses_are_not() {
        let c = config("s3cret");
        assert!(authorized(&c, "Bearer s3cret"));
        assert!(!authorized(&c, "Bearer s3cre"));
        assert!(!authorized(&c, "Bearer s3crett"));
        assert!(!authorized(&c, "bearer s3cret"));
        assert!(!authorized(&c, "s3cret"));
    }

    #[test]
    fn a_non_ascii_header_is_rejected_rather_than_crashing() {
        // The reference hit this as its issue #163: a latin-1-decoded header
        // containing non-ASCII crashed its comparison with a 500 instead of
        // returning 401. Comparing bytes sidesteps it entirely.
        let c = config("s3cret");
        assert!(!authorized(&c, "Bearer é"));
        assert!(!authorized(&c, "Bearer \u{fffd}"));
    }

    #[test]
    fn constant_time_eq_agrees_with_ordinary_equality() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"ab", b"abc"));
    }

    #[test]
    fn only_health_is_public() {
        assert!(!requires_auth("/health"));
        for path in [
            "/stats",
            "/count",
            "/metrics",
            "/admin/compact_tombstones",
            "/sync/push",
            "/sync/pull",
            "/sync/pull_entities",
            "/sync/pull_links",
            "/sync/pull_entity_relations",
        ] {
            assert!(requires_auth(path), "{path} must require auth");
        }
    }
}
