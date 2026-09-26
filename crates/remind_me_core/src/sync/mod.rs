//! Multi-node sync: push the local outbox to a hub, pull its changes back.
//!
//! See `docs/adr/0004-sync-protocol-and-conflict-resolution.md`,
//! `docs/adr/0005-graph-sync.md`, and `docs/adr/0006-peer-discovery.md` for
//! what this module does and does not implement yet. In short: `memories`
//! and the knowledge-graph tables (`entities`/`entity_relations`/
//! `memory_entities`) sync against a configured hub and every discovered
//! peer (a static list plus Tailscale's local API); no OAuth or
//! `remind_me_revoke_clients` yet — deferred to its own follow-up issue,
//! exactly as the epic asked for this to be split.
//!
//! Off unless [`NODE_ID_ENV`], [`HUB_URL_ENV`], and [`SYNC_SECRET_ENV`] are
//! all set — the same default-off posture as the webhook endpoint (`#56`)
//! and the folder watcher (`#55`).

use crate::db::outbox::Outbox;
use crate::db::sync_state::SyncState;
use chrono::{Duration, Utc};
use rusqlite::{Connection, Result};

mod graph;
// Public so `notifications` can reuse the one HTTP client this crate has
// rather than growing a second.
pub mod http;
mod peers;
mod pull;
mod push;
mod reconcile;
mod record;
mod server;
mod status;
mod worker;

pub use graph::{
    apply_incoming_record, upsert_entity_record, upsert_entity_relation_record, upsert_link_record,
    EntityRelationSyncRecord, EntitySyncRecord, GraphApplyError, LinkSyncRecord,
};
pub use peers::{discover_peers, probe_peer, Peer, STATIC_PEERS_ENV, TAILSCALE_SOCKET_ENV};
pub use pull::{
    pull_entities, pull_entity_relations, pull_links, pull_remote, PullError, PullReport,
};
pub use push::{push_outbox, PushError, PushReport};
pub use reconcile::{classify, reconcile, reconcile_hub, reconcile_peer, PULL_LAG_GRACE_SECONDS};
pub use record::{upsert_record, ApplyOutcome, SyncApplyError, SyncRecord};
pub use server::{serve_once, PeerServer, PeerServerConfig, PeerServerStatus, SyncPeer};
pub use status::{sync_repair, sync_status};
pub use worker::{
    disabled_status as sync_worker_disabled_status, live_status_against as sync_live_status,
    superseded as sync_error_superseded, SyncWorker, SyncWorkerStatus, HUB_REMOTE_ID,
};

/// This node's identity in sync records. Empty (the default) means "no
/// identity configured" — matching the reference's own `NODE_ID = ""`
/// default, stamped onto every locally-created memory regardless of
/// whether sync is actually enabled, so a node that turns sync on later
/// does not have to guess which of its existing memories were its own.
pub const NODE_ID_ENV: &str = "REMIND_ME_NODE_ID";
/// A human-readable label for this device/install, stamped alongside
/// `node_id`. Defaults to `"unknown"`, matching the reference.
pub const CLIENT_ENV: &str = "REMIND_ME_CLIENT";
/// The hub this node pushes to and pulls from. Sync is off without one.
pub const HUB_URL_ENV: &str = "REMIND_ME_HUB_URL";
/// Bearer token required on every `/sync/push` and `/sync/pull` request,
/// both sent (to the hub) and required (of callers of this node's own peer
/// server). Sync is off without one.
pub const SYNC_SECRET_ENV: &str = "REMIND_ME_SYNC_SECRET";
/// Seconds between background sync cycles.
pub const SYNC_INTERVAL_ENV: &str = "REMIND_ME_SYNC_INTERVAL";
pub const DEFAULT_SYNC_INTERVAL_SECS: u64 = 60;
/// Bind address for this node's own peer server (accepting another node's
/// push/pull). Defaults to all interfaces — unlike the webhook endpoint,
/// a sync peer server has to be reachable by other machines to be useful
/// at all, so there is no safe localhost-only default to fall back on.
pub const PEER_BIND_ENV: &str = "REMIND_ME_PEER_BIND";
pub const DEFAULT_PEER_BIND: &str = "0.0.0.0";
pub const PEER_PORT_ENV: &str = "REMIND_ME_PEER_PORT";
pub const DEFAULT_PEER_PORT: u16 = 8766;

pub const DEFAULT_CLIENT: &str = "unknown";

/// The literal text a `${user_config.KEY}` reference in an MCP client's
/// `env` config would leave behind if that substitution were attempted
/// against an unfilled value, rather than falling back to an empty string --
/// observed directly wiring a Claude Code plugin's `userConfig` into this
/// plugin's own `.mcp.json`, an approach abandoned after confirming neither
/// of Claude Code's two documented delivery paths (manifest-level
/// `${user_config.*}` substitution, or `CLAUDE_PLUGIN_OPTION_<KEY>` env
/// export) actually reaches an MCP server subprocess, only hook processes.
/// Kept as a defensive guard regardless of that history: none of
/// `NODE_ID_ENV`/`HUB_URL_ENV`/`SYNC_SECRET_ENV` is ever legitimately this
/// shape, so a value here means "never configured", not a hub URL/secret to
/// actually use -- without this check, `sync_enabled` would read it as a
/// real (if unusual) value and turn sync on against a garbage endpoint
/// instead of leaving it off.
fn is_unresolved_placeholder(value: &str) -> bool {
    value.starts_with("${user_config.")
}

/// Read a sync config var, treating both "unset" and an unresolved
/// `${user_config.*}` placeholder (see [`is_unresolved_placeholder`]) as
/// `""`.
fn configured_env(key: &str) -> String {
    let value = std::env::var(key).unwrap_or_default();
    if is_unresolved_placeholder(&value) {
        String::new()
    } else {
        value
    }
}

/// This node's configured identity, or `""` if unset — stamped on every
/// newly created memory regardless of whether sync is enabled, matching
/// the reference's own `memory_add` exactly (`NODE_ID`/`CLIENT` are plain
/// module-level config, read unconditionally).
pub fn configured_node_id() -> String {
    configured_env(NODE_ID_ENV)
}

/// Identity reported by the MCP client in its `initialize` handshake.
///
/// Process-global because the identity is a property of the connection, not
/// of any one call, and threading it from the JSON-RPC layer down through
/// every write path would touch far more code than it informs. The same shape
/// the `REMIND_ME_*` config it overrides already has.
static HANDSHAKE_CLIENT: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Record the client identity from an MCP `initialize`.
///
/// Called by the server on handshake. `None` clears it, which is what a fresh
/// process — or a test — wants.
pub fn set_handshake_client(identity: Option<String>) {
    if let Ok(mut slot) = HANDSHAKE_CLIENT.write() {
        *slot = identity.filter(|s| !s.trim().is_empty());
    }
}

/// The client identity currently in force.
pub fn handshake_client() -> Option<String> {
    HANDSHAKE_CLIENT.read().ok().and_then(|s| s.clone())
}

/// Who is writing, for the `client` column.
///
/// Precedence: the MCP handshake identity, then [`CLIENT_ENV`], then
/// `"unknown"`.
///
/// The handshake wins because it is *observed* rather than *configured* — it
/// says which agent actually made this call, where the environment variable
/// says only what whoever launched the process typed. On a host running one
/// server for several clients the env value is the same for all of them, so
/// preferring it would record the least informative of the two. An operator
/// who deliberately sets `REMIND_ME_CLIENT` still gets it for every non-MCP
/// path — the CLI, the dashboard, the importer — none of which handshake.
pub fn configured_client() -> String {
    if let Some(identity) = handshake_client() {
        return identity;
    }
    std::env::var(CLIENT_ENV).unwrap_or_else(|_| DEFAULT_CLIENT.to_string())
}

/// The `(node_id, client)` pair every newly created memory is stamped with.
///
/// One function rather than two calls at each site, because the bug this
/// exists to close was **omission**, not a wrong value: `add_memory` set both
/// columns and the five other paths that insert into `memories` directly
/// (`capture`, its decompose half, `promotion`, `skeleton`, `normalize`) set
/// neither. `client` fell back to the schema default `'unknown'` and `node_id`
/// to `NULL`.
///
/// That is worse than not having the columns. A reader of `client` could not
/// tell "unknown because nobody configured one" from "unknown because this
/// write path forgot", and `node_id` rides the outbox payload
/// (`schema_triggers.sql`), so per-node attribution on the hub silently saw
/// only manually-added memories.
pub fn memory_provenance() -> (String, String) {
    (configured_node_id(), configured_client())
}

fn configured_hub_url() -> String {
    configured_env(HUB_URL_ENV)
}

fn configured_sync_secret() -> String {
    configured_env(SYNC_SECRET_ENV)
}

/// `NODE_ID`, `HUB_URL`, and `SYNC_SECRET` are all non-empty — matching the
/// reference's `SYNC_ENABLED = bool(NODE_ID and HUB_URL and SYNC_SECRET)`
/// exactly. Gates: whether `delete_memory` tombstones instead of hard
/// deleting, whether the background sync worker does anything, and (via
/// [`SyncPeer::from_env`]'s own additional secret-only check) whether this
/// node's peer server binds a port at all.
pub fn sync_enabled() -> bool {
    !configured_node_id().is_empty()
        && !configured_hub_url().is_empty()
        && !configured_sync_secret().is_empty()
}

/// How long a probed hub version stays fresh.
///
/// Several dashboard tabs refreshing must not turn a page load into hub
/// traffic; a build number does not change between requests anyway.
const HUB_VERSION_TTL: std::time::Duration = std::time::Duration::from_secs(60);

static HUB_VERSION_CACHE: std::sync::Mutex<Option<(std::time::Instant, Option<String>)>> =
    std::sync::Mutex::new(None);

/// The hub's build, probed from its `/health`.
///
/// `None` when sync is not configured, when the hub is unreachable, or when it
/// reports no version — all three are the same thing to a caller: no line to
/// render. Deliberately best-effort: a version display must never be able to
/// fail the page it decorates.
///
/// **Probed live rather than read from sync state.** The reference makes this
/// argument and it applies here too: a dashboard started standalone never runs
/// a sync cycle, so anything populated by that cycle would report `null`
/// forever while looking like a working feature. Asking the hub is correct
/// regardless of which process is serving the page.
///
/// Wrapped here rather than exposing `sync::http`: the API crate has no
/// business making raw HTTP requests, and this keeps the auth header and the
/// cache in one place.
pub fn probe_hub_version() -> Option<String> {
    if !sync_enabled() {
        return None;
    }

    let mut cache = HUB_VERSION_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, value)) = cache.as_ref() {
        if at.elapsed() < HUB_VERSION_TTL {
            return value.clone();
        }
    }

    let url = format!("{}/health", configured_hub_url().trim_end_matches('/'));
    let version = match http::get(&url, &configured_sync_secret()) {
        Ok((200, body)) => serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| {
                v.get("version")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            }),
        _ => None,
    };

    *cache = Some((std::time::Instant::now(), version.clone()));
    version
}

/// The `sync_flags` key recording whether the outbox is being filled.
const SYNC_ENABLED_FLAG: &str = "sync_enabled";

/// Align `sync_flags.sync_enabled` with [`sync_enabled`], every time the
/// schema is opened — matching the reference's own `_reconcile_sync_enabled_flag`,
/// called on every startup, verbatim:
///
/// - already matches: no-op.
/// - stored `"0"`, now enabled: the outbox is backfilled with an `insert` row
///   for every current `memories`/`entities`/`memory_entities` row, so
///   changes made while sync was off still reach a remote once it's
///   configured. Matches the reference exactly in NOT backfilling
///   `entity_relations` either — preserved rather than "fixed," since
///   covering more tables than the reference does would make the two diverge
///   on what a first sync actually sends.
/// - now disabled (from any prior state): `sync_outbox`/`sync_sends` are
///   cleared — nothing is left to drain.
/// - unset (a fresh database) and now enabled: no backfill, matching the
///   reference's own reasoning verbatim even though the reference's stated
///   justification ("pre-gate triggers were unconditional, so the outbox is
///   already complete") describes reference history this crate never had.
///   Reproducing the exact stored/desired matrix rather than the reasoning
///   behind one cell of it keeps this one reconciliation function, not two
///   diverging ones for "true fresh" vs. "upgraded from an older,
///   once-ungated build."
pub fn reconcile_sync_enabled_flag(conn: &Connection) -> Result<()> {
    let desired = if sync_enabled() { "1" } else { "0" };
    let state = SyncState::new(conn);
    let stored = state.flag(SYNC_ENABLED_FLAG)?;

    if stored.as_deref() == Some(desired) {
        return Ok(());
    }

    if desired == "1" && stored.as_deref() == Some("0") {
        Outbox::new(conn).backfill_everything()?;
    } else if desired == "0" {
        Outbox::new(conn).clear()?;
    }

    state.set_flag(SYNC_ENABLED_FLAG, desired)
}

/// Days an unsent outbox row is kept before being pruned.
///
/// Mirrors the reference's `OUTBOX_RETENTION_DAYS`, including the environment
/// variable that overrides it, so a database shared between the two systems is
/// governed by one policy rather than two.
pub const DEFAULT_OUTBOX_RETENTION_DAYS: i64 = 30;
const OUTBOX_RETENTION_ENV: &str = "REMIND_ME_OUTBOX_RETENTION_DAYS";

fn outbox_retention_days() -> i64 {
    std::env::var(OUTBOX_RETENTION_ENV)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|d| *d >= 0)
        .unwrap_or(DEFAULT_OUTBOX_RETENTION_DAYS)
}

/// Prune already-sent outbox rows and anything past the retention window.
///
/// Returns the number of rows removed.
///
/// # Why this runs at all
///
/// `memories_outbox_ai` and `memories_outbox_au` fire on every insert and every
/// update of `memories`, writing a full JSON snapshot of the row. Since
/// retrieval records access — which is an `UPDATE` — the outbox grows on reads
/// as well as writes. Nothing in this crate drains it, so without a prune it
/// grows without bound, carrying a copy of every memory's content each time.
///
/// # Why this policy and not another
///
/// This is the reference's own rule, verbatim: rows already marked sent are
/// echo-suppressed and never pushed, so they go immediately; the rest are kept
/// for the retention window so an intermittently-reachable remote can still
/// catch up, then dropped along with their per-remote send markers.
///
/// Copying the policy rather than inventing one matters because a database can
/// be shared with `remind_me` — it opens the same file and prunes on the same
/// rule, so anything this deletes is something the reference would have deleted
/// too. A tighter rule here would silently drop changes the reference still
/// intended to push.
///
/// # Where it runs
///
/// The reference prunes on each sync cycle. This runs both on database open
/// (bounding a long-lived database even with sync disabled) and, when sync
/// is enabled, once per [`SyncWorker`] cycle — matching the reference's own
/// arrangement now that one exists.
pub fn prune_outbox(conn: &Connection) -> Result<usize> {
    let cutoff = (Utc::now() - Duration::days(outbox_retention_days())).to_rfc3339();
    Outbox::new(conn).prune(&cutoff)
}

/// Records a real, successful HTTP push round trip with `remote_id` (a push
/// destination like `"hub"`) -- setting `last_attempt_at` and
/// `last_push_at`. Only ever called with a bare destination id: pushing
/// happens once per remote for the whole `sync_outbox`, not per resource
/// type, so there is no namespaced push cursor the way pulls have one (see
/// [`record_pull`] for those).
///
/// `sync_status`'s and `reconcile`'s docs both promise these `_at` columns
/// "advance every cycle" so a quiet-but-healthy remote is distinguishable
/// from a wedged one — a promise nothing wrote until now, which left every
/// remote reading as permanently stuck at whatever value it last held
/// (`1970-01-01` for one that has genuinely never answered, and otherwise
/// whatever the last write happened to be, however old). Best-effort: a
/// write failure here is telemetry, not correctness, and must not turn a
/// successful sync into a reported failure.
pub(crate) fn record_push(conn: &Connection, remote_id: &str) {
    let _ = SyncState::new(conn).stamp_push(remote_id, &Utc::now().to_rfc3339());
}

/// Same as [`record_push`], but for a successful pull -- sets
/// `last_attempt_at` and `last_pull_at` instead.
pub(crate) fn record_pull(conn: &Connection, remote_id: &str) {
    let _ = SyncState::new(conn).stamp_pull(remote_id, &Utc::now().to_rfc3339());
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global; serialize this module's env-touching
    // tests so they don't race each other the way `cargo test` otherwise
    // would. `pub(crate)` (rather than a second, module-local lock) because
    // `sync::server`'s tests mutate this same `SYNC_SECRET_ENV` -- two
    // independent locks over one shared process global would still race
    // each other, which is exactly what happened before this was unified
    // (a `combined-features` CI run hit
    // `sync::server::tests::unresolved_user_config_placeholder_does_not_start_a_peer_server`
    // failing because a concurrent `sync::tests` test set a real secret in
    // the assertion's window).
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        crate::test_env::remove_var(NODE_ID_ENV);
        crate::test_env::remove_var(HUB_URL_ENV);
        crate::test_env::remove_var(SYNC_SECRET_ENV);
    }

    /// A stray literal `${user_config.KEY}` string in one of these vars --
    /// what an MCP client's `env` config substitution would leave behind if
    /// it were ever attempted and failed to resolve -- must not read as a
    /// real (if unusual) hub URL/node id/secret. `sync_enabled` must treat
    /// it exactly like the var being unset, and never turn sync on against a
    /// garbage endpoint.
    #[test]
    fn unresolved_user_config_placeholder_does_not_enable_sync() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(NODE_ID_ENV, "${user_config.node_id}");
        crate::test_env::set_var(HUB_URL_ENV, "${user_config.hub_url}");
        crate::test_env::set_var(SYNC_SECRET_ENV, "${user_config.sync_secret}");

        assert!(!sync_enabled());
        assert_eq!(configured_node_id(), "");
        assert_eq!(configured_hub_url(), "");
        assert_eq!(configured_sync_secret(), "");

        clear_env();
    }

    /// A real, fully-configured triple still enables sync -- the placeholder
    /// guard must not reject legitimate values that simply start with `$`
    /// or otherwise resemble the pattern by coincidence.
    #[test]
    fn a_real_triple_still_enables_sync() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        crate::test_env::set_var(NODE_ID_ENV, "my-laptop");
        crate::test_env::set_var(HUB_URL_ENV, "https://hub.example.com");
        crate::test_env::set_var(SYNC_SECRET_ENV, "s3cr3t");

        assert!(sync_enabled());
        assert_eq!(configured_node_id(), "my-laptop");
        assert_eq!(configured_hub_url(), "https://hub.example.com");
        assert_eq!(configured_sync_secret(), "s3cr3t");

        clear_env();
    }
}
