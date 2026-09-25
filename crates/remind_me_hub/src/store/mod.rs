//! The storage interface, and the backends behind it.
//!
//! # Why a trait at all
//!
//! The reference hub is Postgres-only. This one runs on either Postgres or
//! SQLite, and `docs/adr/0015` records why: a hub that cannot take over an
//! existing Postgres deployment is not a successor, and a hub that *requires*
//! Postgres is a heavy ask of the single-operator self-host case the SQLite
//! node already serves happily. `docs/adr/0021` adds a third, `multimodal`
//! (behind the `multimodal-store` feature), which is to replace both.
//!
//! # What the trait deliberately does not expose
//!
//! No connections, no transactions, no SQL. Every method is one complete
//! operation, because the backends differ in exactly the places a leakier
//! interface would have to paper over: sequences (`nextval` vs. `MAX(...)+1`),
//! upsert syntax, JSONB vs. TEXT-holding-JSON, planner statistics that only
//! one of them has.
//!
//! [`HubStore::apply_record`] is the sharpest case. The reference wraps each
//! record in its own savepoint so one malformed record cannot poison a batch,
//! and *that isolation is part of the operation*, not something a caller can
//! be trusted to remember. So the trait takes one already-validated
//! [`Record`] and owns the transaction around it.

use crate::record::Record;
use std::collections::BTreeMap;

pub mod sqlite;

#[cfg(feature = "postgres-store")]
pub mod postgres;

#[cfg(feature = "multimodal-store")]
pub mod multimodal;

/// Anything that went wrong talking to storage.
///
/// Deliberately a string rather than a backend-specific error type: it crosses
/// a trait boundary two very different drivers implement, and every caller
/// either logs it or turns it into a 500. Where the *shape* of a failure
/// matters — a connection that is down, as opposed to a query that failed —
/// [`HubStore::ping`] answers that question directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

pub type StoreResult<T> = Result<T, StoreError>;

/// Tables `/count` will report, and the only values its `table` filter takes.
///
/// An allowlist, not a validated identifier: the name is interpolated into SQL
/// text because a table name is not a parameterisable position in either
/// backend, so it must never come from the request.
pub const COUNTABLE: [&str; 4] = [
    "memories",
    "entities",
    "memory_entities",
    "entity_relations",
];

/// Server-side cap on a pull page, whatever a caller asks for.
pub const MAX_PULL_LIMIT: usize = 500;

/// Cap on records per `/sync/push`.
///
/// A client that stops honouring `processed_ids` retries a growing backlog;
/// without this it could post its entire outbox as one body, which is an OOM
/// vector from a merely buggy — not malicious — client.
pub const MAX_PUSH_BATCH: usize = 1000;

/// Memory counts, which carry a live/tombstone split the other tables lack.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryCounts {
    pub total: i64,
    /// `None` when the number was not computed — approximate counts have no
    /// filtered equivalent, and `since` counts deliberately do not split.
    pub live: Option<i64>,
    pub tombstones: Option<i64>,
}

/// One `/count` or `/metrics` answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Counts {
    pub memories: Option<MemoryCounts>,
    pub entities: Option<i64>,
    pub memory_entities: Option<i64>,
    pub entity_relations: Option<i64>,
}

/// The `/stats` aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stats {
    pub total: i64,
    pub tombstones: i64,
    pub oldest_updated_at: Option<String>,
    pub newest_updated_at: Option<String>,
    /// Ordered by count descending, then name, so the JSON is stable across
    /// calls and backends — Postgres and SQLite do not agree on the order of
    /// equal-count groups otherwise, which would make a diff of two /stats
    /// responses noisy for no reason.
    pub by_origin_node: Vec<(String, i64)>,
    pub by_category: Vec<(String, i64)>,
    pub entities: i64,
    pub memory_entities: i64,
    pub entity_relations: i64,
}

/// Which cursor mode a `/sync/pull` request asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullCursor {
    /// Keyset on the hub-assigned monotonic sequence. Immune to the
    /// late-push problem the timestamp modes have.
    Seq(i64),
    /// Legacy `(updated_at, id)` keyset.
    Keyset { since: String, since_id: String },
    /// Legacy strict `updated_at >`.
    Since(String),
}

/// A `/sync/pull` request, after query parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullQuery {
    pub cursor: PullCursor,
    pub exclude_node: Option<String>,
    /// Drops `exclude_node` regardless, so a node that lost its database can
    /// re-seed everything it originally authored.
    pub full: bool,
    pub limit: usize,
}

/// A keyset pull over one of the immutable graph tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPullQuery {
    pub since: String,
    pub since_id: String,
    pub limit: usize,
}

/// Clamp a caller's requested page size into the server's range.
pub fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_PULL_LIMIT)
}

/// Storage for one hub.
///
/// Implementations must be safe to call from several request threads at once;
/// each method is one self-contained unit of work.
pub trait HubStore: Send + Sync {
    /// Create the schema, and upgrade a legacy database in place.
    fn migrate(&self) -> StoreResult<()>;

    /// Cheapest possible "is the database reachable" probe, for `/health`.
    fn ping(&self) -> StoreResult<()>;

    /// Apply one validated record, in its own transaction.
    ///
    /// Returns whether the record changed anything — false for an LWW loss or
    /// an insert-or-ignore that hit an existing row, which is *not* a failure
    /// and is reported separately from one.
    fn apply_record(&self, record: &Record, origin: Option<&str>) -> StoreResult<bool>;

    fn stats(&self) -> StoreResult<Stats>;

    /// Exact counts, by scan.
    fn count_tables(&self, wanted: &[&str]) -> StoreResult<Counts>;

    /// Planner-estimated counts. `None` when the backend has no cheap
    /// estimate to offer, which is the honest answer rather than a scan
    /// wearing an `approximate` label.
    fn approx_count_tables(&self, wanted: &[&str]) -> StoreResult<Option<Counts>>;

    /// Counts restricted to records touched since `since`.
    fn count_tables_since(&self, wanted: &[&str], since: &str) -> StoreResult<Counts>;

    /// Per-pushing-node memory counts — the one hub-only breakdown, since
    /// `origin_node` never crosses the wire.
    fn count_by_origin_node(&self, since: Option<&str>) -> StoreResult<Vec<(String, i64)>>;

    /// Per-category memory counts, for `remind_me_sync_reconcile`'s
    /// category-by-category drift check against `/count?by=category`.
    fn count_by_category(&self, since: Option<&str>) -> StoreResult<Vec<(String, i64)>>;

    /// Hard-delete memories tombstoned before `cutoff`. Returns how many.
    fn compact_tombstones(&self, cutoff: &str) -> StoreResult<usize>;

    fn pull_memories(&self, query: &PullQuery) -> StoreResult<Vec<serde_json::Value>>;
    fn pull_entities(&self, query: &PullQuery) -> StoreResult<Vec<serde_json::Value>>;
    fn pull_links(&self, query: &GraphPullQuery) -> StoreResult<Vec<serde_json::Value>>;
    fn pull_entity_relations(&self, query: &GraphPullQuery) -> StoreResult<Vec<serde_json::Value>>;
}

/// Sort a group-by result into the stable order [`Stats`] documents.
pub(crate) fn stable_group_order(groups: BTreeMap<String, i64>) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = groups.into_iter().collect();
    // Count descending, then name ascending. The name tiebreak is what makes
    // this deterministic; without it two backends (or two calls) can disagree
    // about equal-count groups.
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// The label the reference uses for a memory with no `origin_node`.
pub(crate) const UNATTRIBUTED: &str = "(unattributed)";
/// The label the reference uses for a memory with no category.
pub(crate) const NO_CATEGORY: &str = "(none)";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_clamped_into_range_at_both_ends() {
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(50), 50);
        assert_eq!(clamp_limit(10_000), MAX_PULL_LIMIT);
    }

    #[test]
    fn group_order_is_count_descending_then_name() {
        let mut groups = BTreeMap::new();
        groups.insert("b".to_string(), 2);
        groups.insert("a".to_string(), 2);
        groups.insert("c".to_string(), 9);
        assert_eq!(
            stable_group_order(groups),
            vec![
                ("c".to_string(), 9),
                ("a".to_string(), 2),
                ("b".to_string(), 2),
            ]
        );
    }
}
