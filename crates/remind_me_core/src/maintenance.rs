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

use crate::db::curation::{Backlog, Curation};
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
    backlog: Backlog,
}

const QUEUES: &[Queue] = &[
    Queue {
        key: "undecomposed_captures",
        label: "captures not decomposed into facts",
        prompt: "decompose_facts",
        backlog: Backlog::Undecomposed,
    },
    Queue {
        key: "unannotated_memories",
        label: "memories with no entity/triple annotation",
        prompt: "backfill_graph",
        backlog: Backlog::Unannotated,
    },
    Queue {
        key: "unnormalized_imports",
        label: "raw imports not normalized",
        prompt: "normalize_imports",
        backlog: Backlog::Unnormalized,
    },
    Queue {
        key: "unclassified_memories",
        label: "memories unclassified",
        prompt: "classify_memories",
        backlog: Backlog::Unclassified,
    },
];

/// Depth of every maintenance queue.
///
/// Never fails: a queue whose query errors reports 0 rather than propagating,
/// because a status helper must not be the thing that breaks a search.
pub fn pending_counts(conn: &Connection) -> HashMap<String, i64> {
    let curation = Curation::new(conn);
    let mut counts = HashMap::new();
    for queue in QUEUES {
        let count = curation.backlog_depth(queue.backlog).unwrap_or(0);
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
    let (captures, last_capture_at) = Curation::new(conn)
        .capture_activity()
        .map(|a| (a.captures, a.last_capture_at))
        .unwrap_or((0, None));
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
