//! Maintenance queue depths, capture health, and the throttled nudge.
//!
//! # Why a nudge exists at all
//!
//! Every one of these backlogs was already computable, and every one was only
//! reachable from a status tool a conversational session has no reason to
//! call. A growing pile of undecomposed captures was therefore invisible in
//! practice. The nudge puts it on a surface that actually gets read.
//!
//! # The throttle slot is claimed *before* the counts run
//!
//! Seven `COUNT(*)`s on the search hot path would be a real cost, and the
//! obvious ordering — count, then decide whether to emit — pays it on every
//! single call. Claiming the timer first bounds how often the *work* happens,
//! not just how often a notice appears, so a quiet vault costs the same as a
//! busy one.
//!
//! Timers are keyed rather than global. The maintenance nudge and any other
//! advisory are independent, with different cadences, and one claiming a
//! single shared slot would silently suppress the other.
//!
//! # A failing count is reported as zero, never propagated
//!
//! These are status helpers. On a partially-migrated database a missing table
//! would otherwise make an advisory the thing that breaks a search — an
//! absurd trade. A queue that cannot be counted reports 0 and the rest still
//! report honestly.
//!
//! # Capture health answers a question silence cannot
//!
//! `auto_capture` only runs when the user has pasted the opt-in instruction
//! into their client. A client where that never happened is indistinguishable
//! from one where it did but nothing was worth capturing — both produce
//! silence. Reporting the count and the last capture time makes "never
//! configured" a visible state rather than something to infer.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Seconds between maintenance *checks*.
pub const NUDGE_INTERVAL_SECONDS: u64 = 3600;

/// A queue must be at least this deep before it is worth mentioning.
pub const NUDGE_THRESHOLD: i64 = 25;

/// At most this many backlogs are named in one nudge. A list of seven is a
/// wall of text nobody acts on; the three deepest are a decision.
pub const NUDGE_MAX_QUEUES: usize = 3;

/// Environment switch, matching the reference's opt-out.
pub const NUDGES_ENABLED_ENV: &str = "REMIND_ME_MAINTENANCE_NUDGES";

/// One maintenance queue: how to count it, what to call it, and which prompt
/// drains it.
struct Queue {
    key: &'static str,
    label: &'static str,
    prompt: &'static str,
    sql: &'static str,
}

/// Captures with no decomposed facts pointing back at them.
const UNDECOMPOSED: &str = "SELECT COUNT(*) FROM memories m
     WHERE m.capture_id IS NOT NULL
       AND m.source_capture_id IS NULL
       AND m.deleted_at IS NULL
       AND NOT EXISTS (
           SELECT 1 FROM memories c WHERE c.source_capture_id = m.capture_id
       )";

/// Eligible for entity/triple annotation: not superseded, not a raw verbatim
/// dialog (the summary gets annotated instead), and carrying neither a triple
/// nor any entity mention.
const UNANNOTATED: &str = "SELECT COUNT(*) FROM memories m
     WHERE m.superseded_by IS NULL
       AND m.deleted_at IS NULL
       AND m.category != 'dialog'
       AND m.subject IS NULL AND m.predicate IS NULL AND m.object IS NULL
       AND NOT EXISTS (
           SELECT 1 FROM memory_entities me WHERE me.memory_id = m.id
       )";

/// Raw imports with nothing pointing back at them via `normalized_from`.
///
/// `NOT IN` over an uncorrelated subquery rather than `NOT EXISTS`: correlated,
/// SQLite re-scans the index once per candidate row, which on a large vault is
/// a per-row scan rather than a seek. The set form materialises once and probes
/// per row — same answer, dramatically cheaper.
const UNNORMALIZED: &str = "SELECT COUNT(*) FROM memories m
     WHERE m.superseded_by IS NULL
       AND m.deleted_at IS NULL
       AND m.source IN ('document_import', 'chat_import')
       AND m.id NOT IN (
           SELECT json_extract(metadata, '$.normalized_from') FROM memories
           WHERE json_extract(metadata, '$.normalized_from') IS NOT NULL
       )";

const UNCLASSIFIED: &str = "SELECT COUNT(*) FROM memories m
     WHERE m.memory_type = 'unclassified' AND m.deleted_at IS NULL";

const QUEUES: &[Queue] = &[
    Queue {
        key: "undecomposed_captures",
        label: "captures not decomposed into facts",
        prompt: "decompose_facts",
        sql: UNDECOMPOSED,
    },
    Queue {
        key: "unannotated_memories",
        label: "memories with no entity/triple annotation",
        prompt: "backfill_graph",
        sql: UNANNOTATED,
    },
    Queue {
        key: "unnormalized_imports",
        label: "raw imports not normalized",
        prompt: "normalize_imports",
        sql: UNNORMALIZED,
    },
    Queue {
        key: "unclassified_memories",
        label: "memories unclassified",
        prompt: "classify_memories",
        sql: UNCLASSIFIED,
    },
];

/// Depth of every maintenance queue.
///
/// Never fails: a queue whose query errors reports 0 rather than propagating,
/// because a status helper must not be the thing that breaks a search.
pub fn pending_counts(conn: &Connection) -> HashMap<String, i64> {
    let mut counts = HashMap::new();
    for queue in QUEUES {
        let count = conn
            .query_row(queue.sql, [], |r| r.get::<_, i64>(0))
            .unwrap_or(0);
        counts.insert(queue.key.to_string(), count);
    }

    // Counted through the tool that owns it rather than re-derived, so the
    // nudge cannot disagree with what draining it actually finds.
    counts.insert(
        "contradiction_candidates".to_string(),
        crate::contradictions::candidate_count(conn).unwrap_or(0),
    );
    counts.insert(
        "recalibration_candidates".to_string(),
        crate::recalibrate::candidate_count(conn).unwrap_or(0),
    );

    counts
}

/// Whether conversation capture is actually happening.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureHealth {
    /// Distinct captures, so the dialog/summary pair one capture writes counts
    /// once rather than twice.
    pub captures: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_capture_at: Option<String>,
    /// The whole point: separates "never configured" from "configured, quiet".
    pub ever_captured: bool,
}

pub fn capture_health(conn: &Connection) -> CaptureHealth {
    let row: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT COUNT(DISTINCT capture_id), MAX(created_at) FROM memories
              WHERE capture_id IS NOT NULL AND deleted_at IS NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    let (captures, last_capture_at) = row.unwrap_or((0, None));
    CaptureHealth {
        captures,
        last_capture_at,
        ever_captured: captures > 0,
    }
}

// ---------------------------------------------------------------------------
// Throttle
// ---------------------------------------------------------------------------

fn throttle() -> &'static Mutex<HashMap<String, Instant>> {
    static THROTTLE: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> =
        std::sync::OnceLock::new();
    THROTTLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Claim the throttle slot for `name`, reporting whether it was due.
///
/// Claiming here rather than at the caller's success path is deliberate: it
/// bounds how often the work *behind* the check runs, not merely how often a
/// notice is emitted.
pub fn due(name: &str, interval_seconds: u64) -> bool {
    let now = Instant::now();
    let mut guard = throttle().lock().unwrap_or_else(|e| e.into_inner());
    match guard.get(name) {
        Some(last) if now.duration_since(*last).as_secs() < interval_seconds => false,
        _ => {
            guard.insert(name.to_string(), now);
            true
        }
    }
}

/// Clear every throttle timer. For tests, which cannot wait an hour.
pub fn reset_throttle() {
    throttle().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

fn nudges_enabled() -> bool {
    !matches!(
        std::env::var(NUDGES_ENABLED_ENV)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Look up a queue's label and prompt by key.
///
/// Returns the key itself as the label for an unknown queue rather than
/// panicking: a nudge naming a queue oddly is a cosmetic problem, and a nudge
/// that crashes a search is not.
fn describe(key: &str) -> (String, &'static str) {
    if let Some(q) = QUEUES.iter().find(|q| q.key == key) {
        return (q.label.to_string(), q.prompt);
    }
    match key {
        "contradiction_candidates" => (
            "possibly-contradictory memory pairs".to_string(),
            "review_contradictions",
        ),
        "recalibration_candidates" => (
            "memories due for an importance review".to_string(),
            "recalibrate_importance",
        ),
        other => (other.to_string(), ""),
    }
}

/// Build the nudge for a set of counts, or `None` when nothing is deep enough.
///
/// Split from [`maybe_notice`] so the selection and wording are testable
/// without a clock or a database.
pub fn render_notice(counts: &HashMap<String, i64>) -> Option<String> {
    let mut backlogs: Vec<(&String, &i64)> = counts
        .iter()
        .filter(|(_, count)| **count >= NUDGE_THRESHOLD)
        .collect();
    if backlogs.is_empty() {
        return None;
    }

    // Deepest first, with the key as a tiebreak so two equal backlogs do not
    // reorder between calls — a nudge that reshuffles for no reason reads as
    // new information.
    backlogs.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));

    let mut lines = vec!["**Maintenance pending** — run when convenient:".to_string()];
    for (key, count) in backlogs.into_iter().take(NUDGE_MAX_QUEUES) {
        let (label, prompt) = describe(key);
        lines.push(format!("- {} {} → `{}` prompt", count, label, prompt));
    }
    Some(lines.join("\n"))
}

/// A maintenance nudge, if one is due.
///
/// Returns `None` when nudges are disabled, when the throttle slot is not yet
/// due, or when no queue has crossed the threshold.
pub fn maybe_notice(conn: &Connection) -> Option<String> {
    if !nudges_enabled() {
        return None;
    }
    // Before the counts, not after — see the module docs.
    if !due("maintenance", NUDGE_INTERVAL_SECONDS) {
        return None;
    }
    render_notice(&pending_counts(conn))
}
