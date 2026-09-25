//! Sync observability: is the backlog draining, and when did each remote last
//! actually answer.
//!
//! # Liveness comes from the `_at` columns, never the cursors
//!
//! `sync_log` carries both `last_pull`/`last_push` (content *cursors* — the
//! high-water mark of what has been exchanged) and `last_pull_at`/
//! `last_push_at`/`last_attempt_at` (wall-clock contact times). They answer
//! different questions and are not interchangeable.
//!
//! A quiet-but-healthy remote advances its `_at` timestamps every cycle while
//! its cursors stand still, because there is nothing new to send. Reading
//! liveness off the cursors would report that remote as stalled — and would
//! report a genuinely wedged one as fine the moment anything happened to move.
//! That is exactly the confusion the v20 migration added these columns to end.
//!
//! # The drain verdict
//!
//! A pending count alone is ambiguous: ten thousand queued rows look identical
//! whether they are draining briskly or the push is wedged. So the previous
//! observation is persisted and the *direction* reported. The first call after
//! a restart establishes a baseline and honestly says `Unknown` rather than
//! guessing.

use super::{configured_hub_url, configured_node_id, configured_sync_secret, sync_enabled};
use crate::models::{DrainVerdict, OutboxStatus, RemoteStatus, SyncStatus, TombstoneStatus};
use rusqlite::{params, Connection, OptionalExtension, Result};

/// What a never-contacted remote's timestamps read as. Not NULL — the columns
/// are `NOT NULL DEFAULT` this — so "never" has to be recognised by value.
const EPOCH: &str = "1970-01-01T00:00:00+00:00";

/// Where the previous outbox observation is kept, so a drain rate can be
/// computed across calls.
const DRAIN_FLAG_KEY: &str = "sync_status_last_observation";

fn pending_to_remote(conn: &Connection, remote_id: &str) -> Result<(i64, Option<String>)> {
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sync_outbox o
          WHERE NOT EXISTS (
              SELECT 1 FROM sync_sends s
               WHERE s.outbox_id = o.id AND s.remote_id = ?
          )",
        params![remote_id],
        |r| r.get(0),
    )?;
    let oldest: Option<String> = conn
        .query_row(
            "SELECT MIN(created_at) FROM sync_outbox o
              WHERE NOT EXISTS (
                  SELECT 1 FROM sync_sends s
                   WHERE s.outbox_id = o.id AND s.remote_id = ?
              )",
            params![remote_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok((pending, oldest))
}

/// Read the previous observation, compute a direction, and record the current
/// one for next time.
fn drain(conn: &Connection, pending: i64) -> Result<(DrainVerdict, Option<f64>)> {
    let now = chrono::Utc::now();
    let previous: Option<String> = conn
        .query_row(
            "SELECT value FROM sync_flags WHERE key = ?",
            params![DRAIN_FLAG_KEY],
            |r| r.get(0),
        )
        .optional()?;

    conn.execute(
        "INSERT INTO sync_flags (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![DRAIN_FLAG_KEY, format!("{}|{}", now.to_rfc3339(), pending)],
    )?;

    if pending == 0 {
        return Ok((DrainVerdict::Idle, Some(0.0)));
    }

    // A baseline has to be established before a direction means anything.
    // Saying so beats inventing a verdict from a single sample.
    let Some(previous) = previous else {
        return Ok((DrainVerdict::Unknown, None));
    };
    let Some((when, count)) = previous.split_once('|') else {
        return Ok((DrainVerdict::Unknown, None));
    };
    let (Ok(when), Ok(count)) = (
        chrono::DateTime::parse_from_rfc3339(when),
        count.parse::<i64>(),
    ) else {
        return Ok((DrainVerdict::Unknown, None));
    };

    // Direction and rate need different things, and conflating them was a real
    // bug: two calls a few hundred microseconds apart elapse 0 whole
    // milliseconds, and gating the verdict on elapsed time reported `Unknown`
    // for a backlog that had visibly not moved. The delta alone answers "is it
    // moving"; only the per-minute figure needs a clock.
    let delta = pending - count;
    let verdict = if delta < 0 {
        DrainVerdict::Draining
    } else if delta > 0 {
        DrainVerdict::Growing
    } else {
        DrainVerdict::Stalled
    };

    let micros = (now - when.with_timezone(&chrono::Utc)).num_microseconds();
    let per_minute = match micros {
        Some(micros) if micros > 0 => Some(delta as f64 / (micros as f64 / 60_000_000.0)),
        // Same instant, or a clock that moved backwards. No rate to report,
        // but the direction still stands.
        _ => None,
    };
    Ok((verdict, per_minute))
}

/// A full sync status snapshot.
pub fn sync_status(conn: &Connection) -> Result<SyncStatus> {
    if !sync_enabled() {
        let mut missing = Vec::new();
        if configured_node_id().is_empty() {
            missing.push(super::NODE_ID_ENV.to_string());
        }
        if configured_hub_url().is_empty() {
            missing.push(super::HUB_URL_ENV.to_string());
        }
        if configured_sync_secret().is_empty() {
            missing.push(super::SYNC_SECRET_ENV.to_string());
        }
        return Ok(SyncStatus::Disabled {
            // Naming the specific variables beats "sync is off": the caller is
            // asking because they expected it to be on.
            hint: format!(
                "set {} to enable sync; the outbox triggers stay gated off \
                 until then, so nothing accumulates in the meantime",
                missing.join(", ")
            ),
            missing,
        });
    }

    let (pending, oldest_pending) = pending_to_remote(conn, super::HUB_REMOTE_ID)?;
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM sync_outbox", [], |r| r.get(0))?;
    let (verdict, per_minute) = drain(conn, pending)?;

    let tombstones: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE deleted_at IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let cutoff = (chrono::Utc::now()
        - chrono::Duration::days(super::DEFAULT_OUTBOX_RETENTION_DAYS))
    .to_rfc3339();
    let compactable: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE deleted_at IS NOT NULL AND deleted_at < ?",
        params![cutoff],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT remote_id, last_attempt_at, last_push_at, last_pull_at
           FROM sync_log ORDER BY remote_id",
    )?;
    let rows: Vec<(String, String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<Vec<_>>>()?;
    drop(stmt);

    // Per-table graph cursors (`"{remote}#entities"`, `"…#links"`,
    // `"…#entity_relations"`) never get their own push -- `sync_with_remote`
    // drains the whole outbox in one call keyed by the *base* remote id, so
    // `sync_sends`/`last_push_at` are never written under the cursor's own
    // key. Reading them with the same `pending_to_remote` lookup as a real
    // push target reports a permanently-full backlog and an eternal-epoch
    // `last_push_at` for graph data that is, in fact, pushed and draining
    // fine alongside `memories` -- a false "the hub is stuck" signal. These
    // rows fall back to the base remote's own push state instead.
    let mut remotes = Vec::with_capacity(rows.len());
    for (remote_id, last_attempt_at, last_push_at, last_pull_at) in &rows {
        let base_id = match remote_id.split_once('#') {
            Some((base, _)) => base,
            None => remote_id.as_str(),
        };
        let (pending, last_push_at) = if base_id == remote_id {
            (pending_to_remote(conn, remote_id)?.0, last_push_at.clone())
        } else {
            let (pending, _) = pending_to_remote(conn, base_id)?;
            let push_at = rows
                .iter()
                .find(|(id, ..)| id == base_id)
                .map(|(_, _, push_at, _)| push_at.clone())
                .unwrap_or_else(|| EPOCH.to_string());
            (pending, push_at)
        };
        remotes.push(RemoteStatus {
            // A never-contacted remote sits at the epoch default rather than
            // NULL, which reads as a very stale timestamp unless called out.
            // This is what distinguishes "never tried" from "tried and
            // failing" — the issue's own acceptance criterion.
            ever_contacted: last_attempt_at != EPOCH,
            remote_id: remote_id.clone(),
            last_attempt_at: last_attempt_at.clone(),
            last_push_at,
            last_pull_at: last_pull_at.clone(),
            pending,
        });
    }

    Ok(SyncStatus::Enabled {
        node_id: configured_node_id(),
        hub_url: configured_hub_url(),
        outbox: OutboxStatus {
            pending,
            sent: total - pending,
            total,
            oldest_pending,
            drain: verdict,
            per_minute,
        },
        tombstones: TombstoneStatus {
            total: tombstones,
            compactable_now: compactable,
        },
        remotes,
    })
}

/// Reset a remote's pull cursors so the next sync re-pulls history.
///
/// Only the *cursors* are reset, never the `_at` liveness columns: those record
/// what actually happened, and rewriting them to force a re-pull would destroy
/// the evidence of when the remote was last reachable — which is the thing you
/// were looking at when you decided a repair was needed.
///
/// Returns whether a row existed to reset. A remote that was never contacted
/// has nothing to repair, and saying so beats silently reporting success.
pub fn sync_repair(conn: &Connection, remote_id: &str) -> Result<bool> {
    let affected = conn.execute(
        "UPDATE sync_log
            SET last_pull = ?, last_pull_id = '', last_pull_seq = ?
          WHERE remote_id = ?",
        // Back to SEQ_UNKNOWN, not to 0: this must also undo a stuck
        // SEQ_UNSUPPORTED, which is how a hub upgraded past the `hub_seq`
        // feature gets picked up. Resetting to 0 would instead assert support
        // that was never established, and a remote that genuinely lacks it
        // would then be pulled with a cursor it ignores — silently back on the
        // legacy path with no record of why.
        params![EPOCH, super::pull::SEQ_UNKNOWN, remote_id],
    )?;
    Ok(affected > 0)
}
