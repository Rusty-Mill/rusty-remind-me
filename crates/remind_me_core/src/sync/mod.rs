//! What is left of multi-node sync: write provenance and a syncing store's
//! own bookkeeping.
//!
//! The hub, peer discovery, push/pull and the background worker are gone
//! (card #746): remind-me runs as one server with no other nodes. Two things
//! stay here until the outbox itself is retired:
//!
//! - **Provenance.** Every write is stamped with this node's id and the
//!   calling client ([`memory_provenance`]); that never depended on sync.
//! - **A store that synced keeps its bookkeeping.** Its `sync_enabled` flag
//!   stays on, so deletes still leave tombstones and writes still queue
//!   outbox rows, pruned by the retention window. Nothing changes in such a
//!   store's data. [`SYNC_DISABLE_ENV`] turns that off explicitly, emptying
//!   the outbox. A store that never synced is unaffected.

use crate::db::outbox::Outbox;
use crate::db::sync_state::SyncState;
use crate::db::Result;
use crate::db::Store;
use chrono::{Duration, Utc};

// Public so `notifications` and `events` share one small HTTP client.
pub mod http;
pub mod retention;

/// This node's identity, stamped on every newly created memory. Empty (the
/// default) means "no identity configured".
pub const NODE_ID_ENV: &str = "REMIND_ME_NODE_ID";
/// A human-readable label for this device/install, stamped alongside
/// `node_id`. Defaults to `"unknown"`, matching the reference.
pub const CLIENT_ENV: &str = "REMIND_ME_CLIENT";
/// Set (`1`, `true`, `yes`, `on`) to stop a store that once synced from
/// keeping sync bookkeeping: the flag goes off and the outbox is emptied.
pub const SYNC_DISABLE_ENV: &str = "REMIND_ME_SYNC_DISABLE";
/// The `content` a deleted memory keeps once its text is dropped
/// (ADR-0024), in a store that syncs.
pub const TOMBSTONE_CONTENT: &str = "(deleted)";
/// The send-marker id the hub's acknowledgements were recorded under;
/// [`prune_outbox`] drops rows it took.
pub const HUB_REMOTE_ID: &str = "hub";

pub const DEFAULT_CLIENT: &str = "unknown";

/// The literal text a `${user_config.KEY}` reference in an MCP client's
/// `env` config would leave behind if that substitution were attempted
/// against an unfilled value, rather than falling back to an empty string --
/// observed directly wiring a Claude Code plugin's `userConfig` into this
/// plugin's own `.mcp.json`, an approach abandoned after confirming neither
/// of Claude Code's two documented delivery paths (manifest-level
/// `${user_config.*}` substitution, or `CLAUDE_PLUGIN_OPTION_<KEY>` env
/// export) actually reaches an MCP server subprocess, only hook processes.
/// Kept as a defensive guard regardless of that history: `NODE_ID_ENV` is
/// never legitimately this shape, so a value here means "never configured".
fn is_unresolved_placeholder(value: &str) -> bool {
    value.starts_with("${user_config.")
}

/// Read a config var, treating both "unset" and an unresolved
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
/// newly created memory, matching the reference's own `memory_add` exactly (`NODE_ID`/`CLIENT` are plain
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
///
/// On a daemon connection it belongs to that connection's session instead:
/// one daemon serves many clients, and a process-wide slot would record
/// whichever of them shook hands last.
pub fn set_handshake_client(identity: Option<String>) {
    let identity = identity.filter(|s| !s.trim().is_empty());
    if crate::daemon::session::set_handshake_client(identity.clone()) {
        return;
    }
    if let Ok(mut slot) = HANDSHAKE_CLIENT.write() {
        *slot = identity;
    }
}

thread_local! {
    /// The caller of the call running on this thread, while [`with_call_client`]
    /// runs it: `Some(None)` is a caller that did not say who it is.
    static CALL_CLIENT: std::cell::RefCell<Option<Option<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Restores the enclosing [`CALL_CLIENT`] when a call ends, even by panic.
struct CallClientGuard(Option<Option<String>>);

impl Drop for CallClientGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        CALL_CLIENT.with(|slot| *slot.borrow_mut() = previous);
    }
}

/// Run `f` as one call made by `identity`.
///
/// For calls that name their own caller, as the remote connector's do: it
/// serves many callers through one handler (and one daemon connection), so
/// neither a process-wide slot nor a connection's session would keep two of
/// them apart. The MCP server runs each such call inside this scope, on
/// whichever thread, in whichever process, executes it.
/// Inside `f` the identity wins over both; `None` reads as a caller that
/// named nobody, falling through to [`CLIENT_ENV`] rather than to whoever
/// shook hands last.
pub fn with_call_client<R>(identity: Option<String>, f: impl FnOnce() -> R) -> R {
    let identity = identity.filter(|s| !s.trim().is_empty());
    let previous = CALL_CLIENT.with(|slot| slot.borrow_mut().replace(identity));
    let _restore = CallClientGuard(previous);
    f()
}

/// Whether the call running on this thread was forwarded by a transport
/// that names its caller (the remote connector): inside
/// [`with_call_client`], whatever identity it carried.
pub fn in_forwarded_call() -> bool {
    CALL_CLIENT.with(|slot| slot.borrow().is_some())
}

/// The client identity currently in force.
pub fn handshake_client() -> Option<String> {
    if let Some(in_call) = CALL_CLIENT.with(|slot| slot.borrow().clone()) {
        return in_call;
    }
    if let Some(in_session) = crate::daemon::session::handshake_client() {
        return in_session;
    }
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
    crate::daemon::session::var(CLIENT_ENV).unwrap_or_else(|| DEFAULT_CLIENT.to_string())
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
/// write path forgot".
pub fn memory_provenance() -> (String, String) {
    (configured_node_id(), configured_client())
}

/// The `sync_flags` key recording whether the outbox is being filled.
const SYNC_ENABLED_FLAG: &str = "sync_enabled";

/// Settle `sync_flags.sync_enabled` every time the schema is opened.
///
/// Nothing turns sync on any more, so a fresh store, or one whose flag is
/// off, is set off with an empty outbox. A store whose flag is on keeps it,
/// with its outbox and tombstones, unless [`SYNC_DISABLE_ENV`] asks for it
/// off: a process may not quietly change how every other process on the
/// store deletes.
pub fn reconcile_sync_enabled_flag(store: &Store<'_>) -> Result<()> {
    let state = SyncState::new(store);
    let stored = state.flag(SYNC_ENABLED_FLAG)?;
    match stored.as_deref() {
        Some("0") => Ok(()),
        Some("1") if !sync_disable_requested() => Ok(()),
        _ => {
            Outbox::new(store).clear()?;
            state.set_flag(SYNC_ENABLED_FLAG, "0")
        }
    }
}

/// Whether this store syncs: its `sync_enabled` flag, which
/// [`reconcile_sync_enabled_flag`] keeps. What decides whether a delete
/// leaves a tombstone, so every process writing one store deletes the same
/// way.
pub fn store_syncs(store: &Store<'_>) -> Result<bool> {
    Ok(SyncState::new(store).flag(SYNC_ENABLED_FLAG)?.as_deref() == Some("1"))
}

/// Whether [`SYNC_DISABLE_ENV`] asks for sync off: `1`, `true`, `yes` or
/// `on`, any case.
fn sync_disable_requested() -> bool {
    matches!(
        configured_env(SYNC_DISABLE_ENV)
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
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
/// Rows already marked sent are echo-suppressed and never pushed, so they go
/// immediately, as in the reference. So do rows the hub has taken: every
/// node pulls them from the hub, and a peer can also pull them from this
/// node's feed, which reads the memories themselves, not the outbox. Keeping
/// them only held a second full copy of every change, including the text
/// of memories deleted since (ADR-0024). The rest are kept for the retention
/// window so an intermittently reachable remote can still catch up, then
/// dropped along with their per-remote send markers. A node without a hub
/// never records a hub send, so for it only the window applies.
///
/// Copying the policy rather than inventing one matters because a database can
/// be shared with `remind_me` — it opens the same file and prunes on the same
/// rule, so anything this deletes is something the reference would have deleted
/// too. A tighter rule here would silently drop changes the reference still
/// intended to push.
///
/// # Where it runs
///
/// On database open, and every hour in a long-lived process
/// ([`retention::start_pruner_for`]), bounding a long-lived database.
pub fn prune_outbox(store: &Store<'_>) -> Result<usize> {
    let cutoff = (Utc::now() - Duration::days(outbox_retention_days())).to_rfc3339();
    Outbox::new(store).prune(&cutoff, HUB_REMOTE_ID)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A stray literal `${user_config.KEY}` string -- what an MCP client's
    /// `env` substitution leaves behind when it fails to resolve -- reads
    /// as no node id at all.
    #[test]
    fn unresolved_user_config_placeholder_is_no_node_id() {
        let _guard = ENV_LOCK.lock().unwrap();
        crate::test_env::set_var(NODE_ID_ENV, "${user_config.node_id}");
        assert_eq!(configured_node_id(), "");
        crate::test_env::set_var(NODE_ID_ENV, "my-laptop");
        assert_eq!(configured_node_id(), "my-laptop");
        crate::test_env::remove_var(NODE_ID_ENV);
    }
}
