//! Reconciling this node's record counts against a remote's.
//!
//! # The verdict is the output, not the numbers
//!
//! Raw deltas are just numbers, and the benign case and the real fault differ
//! only by a sign. Four verdicts:
//!
//! - `InSync` — no drift.
//! - `PullLag` — the remote is ahead and the last pull was recent. This is the
//!   ordinary state of a healthy node between cycles.
//! - `NodeAhead` — **this node holds records the remote does not**, so pushes
//!   are not landing. Checked first, because it is the only direction that
//!   means data is at risk.
//! - `Fault` — the remote is ahead but the last successful pull is stale, or
//!   never happened. That is not lag.
//!
//! # Why "recent pull" is read from `last_pull_at`
//!
//! The same argument as `sync::status`: `last_pull_at` is the wall clock of
//! the last successful pull *attempt*, which a quiet-but-healthy remote
//! advances every cycle even with nothing to send. The `last_pull` cursor does
//! not. Judging lag by the cursor would call every quiet remote a fault.
//!
//! # One classifier, both remote kinds
//!
//! `reconcile_peer` and a hub reconcile share [`classify`] deliberately.
//! "Local greater than remote means pushes are not landing" does not depend on
//! which machine is on the other end, and a second copy is how the two would
//! eventually disagree about what drift means.

use super::{configured_hub_url, configured_sync_secret, sync_enabled};
use crate::models::{CategoryDrift, ReconcileReport, ReconcileVerdict, RemoteCounts};
use rusqlite::{params, Connection, OptionalExtension, Result};

/// How stale a successful pull may be before hub-ahead drift stops reading as
/// ordinary lag. Generous relative to the sync interval, because a single
/// missed cycle is not a fault.
pub const PULL_LAG_GRACE_SECONDS: i64 = 900;

/// Classify drift into a verdict plus hints explaining it.
///
/// `last_pull_age` is seconds since the last successful pull, or `None` when
/// the remote has never been pulled from.
pub fn classify(
    drift: &[CategoryDrift],
    last_pull_age: Option<i64>,
) -> (ReconcileVerdict, Vec<String>) {
    let mut hints = Vec::new();

    // Checked first and unconditionally: this is the only direction where
    // records exist on exactly one machine and nothing is coming to fix it.
    let ahead: Vec<&str> = drift
        .iter()
        .filter(|d| d.delta < 0)
        .map(|d| d.category.as_str())
        .collect();
    if !ahead.is_empty() {
        hints.push(format!(
            "this node holds records the remote does not ({}) — pushes are not \
             landing; check remind_me_sync_status for a stalled outbox",
            ahead.join(", ")
        ));
        return (ReconcileVerdict::NodeAhead, hints);
    }

    if drift.is_empty() {
        return (ReconcileVerdict::InSync, hints);
    }

    // Remote-ahead drift is judged by evidence rather than by guessing which
    // categories "should" be static.
    match last_pull_age {
        None => {
            hints.push(
                "the remote has never been pulled from successfully, so this drift \
                 is not lag — check connectivity and the sync secret"
                    .to_string(),
            );
            (ReconcileVerdict::Fault, hints)
        }
        Some(age) if age > PULL_LAG_GRACE_SECONDS => {
            hints.push(format!(
                "last successful pull was {}s ago (> {}s grace), so the remote \
                 being ahead is not ordinary lag — the pull is not running",
                age, PULL_LAG_GRACE_SECONDS
            ));
            (ReconcileVerdict::Fault, hints)
        }
        Some(age) => {
            hints.push(format!(
                "the remote is ahead and the last pull was {}s ago — ordinary \
                 pull lag",
                age
            ));
            (ReconcileVerdict::PullLag, hints)
        }
    }
}

/// This node's counts, in the shape a remote's `/count` returns.
fn local_counts(conn: &Connection) -> Result<(i64, i64, std::collections::BTreeMap<String, i64>)> {
    let (total, tombstones): (i64, i64) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN deleted_at IS NOT NULL THEN 1 ELSE 0 END), 0)
           FROM memories",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut stmt = conn.prepare(
        "SELECT COALESCE(NULLIF(category, ''), '(none)'), COUNT(*)
           FROM memories GROUP BY 1",
    )?;
    let by_category = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
    Ok((total, tombstones, by_category))
}

/// Seconds since the last successful pull from `remote_id`, or `None` when it
/// has never been pulled from.
fn last_pull_age(conn: &Connection, remote_id: &str) -> Result<Option<i64>> {
    let at: Option<String> = conn
        .query_row(
            "SELECT last_pull_at FROM sync_log WHERE remote_id = ?",
            params![remote_id],
            |r| r.get(0),
        )
        .optional()?;

    let Some(at) = at else { return Ok(None) };
    // The epoch default means "never", not "56 years stale".
    if at == "1970-01-01T00:00:00+00:00" {
        return Ok(None);
    }
    Ok(chrono::DateTime::parse_from_rfc3339(&at)
        .ok()
        .map(|when| (chrono::Utc::now() - when.with_timezone(&chrono::Utc)).num_seconds()))
}

/// Fetch a remote's `/count`, with the per-category breakdown `?by=category`
/// asks for. Without it the response has no `by_category` key at all --
/// `RemoteCounts::by_category` defaults to empty on a missing key rather than
/// failing to deserialise, which is exactly how this used to go unnoticed:
/// every category read as "remote has 0", so `classify` reported every
/// nonempty local category as drift and the verdict was `NodeAhead` on every
/// call, whether or not sync was actually keeping up.
fn fetch_counts(base_url: &str) -> Result<RemoteCounts, String> {
    let url = format!("{}/count?by=category", base_url.trim_end_matches('/'));
    let (status, body) = super::http::get(&url, &configured_sync_secret())
        .map_err(|e| format!("could not reach {}: {}", url, e))?;
    if status != 200 {
        return Err(format!("{} returned {}", url, status));
    }
    serde_json::from_str::<RemoteCounts>(&body)
        .map_err(|e| format!("{} returned an unreadable body: {}", url, e))
}

/// Diff this node against a remote's `/count` and classify the result.
///
/// `remote_id` names the `sync_log` row whose pull age is consulted, so the
/// same function serves the hub and any peer.
pub fn reconcile(conn: &Connection, base_url: &str, remote_id: &str) -> Result<ReconcileReport> {
    if !sync_enabled() {
        return Ok(ReconcileReport::Unavailable {
            reason: "sync is not configured on this node".to_string(),
        });
    }

    let remote = match fetch_counts(base_url) {
        Ok(counts) => counts,
        // A verdict from a remote that could not be reached would be a guess.
        // Reporting the reachability problem is the answer.
        Err(reason) => return Ok(ReconcileReport::Unavailable { reason }),
    };

    let (local_total, local_tombstones, local_categories) = local_counts(conn)?;

    // Only categories that actually disagree are listed; the rest are counted.
    // A hundred agreeing rows would bury the two that matter.
    let mut drift: Vec<CategoryDrift> = Vec::new();
    let mut agreeing = 0usize;
    for (category, local) in &local_categories {
        let remote_count = remote.by_category.get(category).copied().unwrap_or(0);
        if remote_count == *local {
            agreeing += 1;
        } else {
            drift.push(CategoryDrift {
                category: category.clone(),
                local: *local,
                remote: remote_count,
                delta: remote_count - local,
            });
        }
    }
    for (category, remote_count) in &remote.by_category {
        if !local_categories.contains_key(category) {
            drift.push(CategoryDrift {
                category: category.clone(),
                local: 0,
                remote: *remote_count,
                delta: *remote_count,
            });
        }
    }
    drift.sort_by(|a, b| a.category.cmp(&b.category));

    let age = last_pull_age(conn, remote_id)?;
    let (verdict, hints) = classify(&drift, age);

    Ok(ReconcileReport::Compared {
        remote_id: remote_id.to_string(),
        remote_role: remote.role,
        remote_version: remote.version,
        verdict,
        hints,
        local_total,
        remote_total: remote.memories.total,
        local_tombstones,
        remote_tombstones: remote.memories.tombstones,
        drift,
        categories_agreeing: agreeing,
        last_pull_age_seconds: age,
    })
}

/// Reconcile against the configured hub.
pub fn reconcile_hub(conn: &Connection) -> Result<ReconcileReport> {
    reconcile(conn, &configured_hub_url(), super::HUB_REMOTE_ID)
}

/// Reconcile against one discovered peer, by node id.
pub fn reconcile_peer(conn: &Connection, node_id: &str) -> Result<ReconcileReport> {
    let peers = super::discover_peers();
    let Some(peer) = peers.into_iter().find(|p| p.node_id == node_id) else {
        return Ok(ReconcileReport::Unavailable {
            reason: format!(
                "no peer known as '{}' — remind_me_server_status lists the peers \
                 this node has discovered",
                node_id
            ),
        });
    };
    reconcile(conn, &peer.url, node_id)
}
