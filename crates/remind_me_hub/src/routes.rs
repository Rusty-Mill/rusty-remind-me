//! The ten routes, written against [`HubStore`] rather than any backend.
//!
//! Every handler here is a pure function of a request and a store, which is
//! what lets the whole surface be tested against SQLite in-process while the
//! Postgres backend satisfies the same trait.
//!
//! # Auth posture, which is not uniform and should not be
//!
//! `/health` is deliberately unauthenticated: it is what a deploy healthcheck
//! polls, and it must keep answering when the database is down. Everything
//! else is bearer-gated, including `/metrics` — the reference argues that one
//! out explicitly, and the argument is that anyone scraping the hub is already
//! the operator who provisioned the secret, so the credential is in hand
//! rather than newly invented.

use crate::canon::{canon_ts, now_canonical};
use crate::http::{query_flag, query_param, Response};
use crate::record;
use crate::store::{
    clamp_limit, Counts, GraphPullQuery, HubStore, PullCursor, PullQuery, COUNTABLE,
    MAX_PULL_LIMIT, MAX_PUSH_BATCH,
};
use crate::{Config, EPOCH, HUB_VERSION};
use serde_json::{json, Map, Value};

/// Liveness probe. 200 when the database is reachable, 503 when not.
///
/// The status code reflects connectivity so a deploy healthcheck gating on 2xx
/// catches "the hub cannot reach its database" at rollout, rather than always
/// reporting success.
///
/// **The `db` field never carries the raw error.** A driver's connection error
/// typically embeds host, resolved IP, port, database name, username and the
/// specific auth-failure reason, and this route is unauthenticated and
/// commonly fronted by a tunnel reachable from the open internet. The full
/// error is logged server-side instead.
pub fn health(store: &dyn HubStore) -> Response {
    let reachable = match store.ping() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("hub: health check DB connectivity failed: {e}");
            false
        }
    };
    Response::json(
        if reachable { 200 } else { 503 },
        &json!({
            "status": if reachable { "ok" } else { "degraded" },
            "role": "hub",
            "version": HUB_VERSION,
            "db": if reachable { "ok" } else { "unreachable" },
            "time": now_canonical(),
        }),
    )
}

/// Aggregate counts, so reconciliation needs no shell on the hub host.
///
/// `by_origin_node` is the interesting half: `origin_node` is hub-only and
/// never crosses the wire, so this is the only place the "which node pushed
/// what" breakdown can be observed at all.
pub fn stats(store: &dyn HubStore) -> Response {
    match store.stats() {
        Ok(s) => Response::json(
            200,
            &json!({
                "role": "hub",
                "version": HUB_VERSION,
                "memories": {
                    "total": s.total,
                    "tombstones": s.tombstones,
                    "oldest_updated_at": s.oldest_updated_at,
                    "newest_updated_at": s.newest_updated_at,
                    // Keyed by name so a client can diff against its own
                    // counts without positional assumptions.
                    "by_origin_node": pairs_to_object(&s.by_origin_node),
                    "by_category": pairs_to_object(&s.by_category),
                },
                "entities": s.entities,
                "memory_entities": s.memory_entities,
                "entity_relations": s.entity_relations,
                "time": now_canonical(),
            }),
        ),
        Err(e) => storage_error("stats", &e.0),
    }
}

/// Scalar counts — the cheap subset of `/stats`, safe to poll.
///
/// Deliberately query-shaped rather than response-shaped: it never groups
/// unless asked, and `?table=` narrows it to a single count so a poller does
/// not scan graph tables it is not watching.
pub fn count(store: &dyn HubStore, query: &str) -> Response {
    let table = query_param(query, "table");
    if let Some(t) = &table {
        if !COUNTABLE.contains(&t.as_str()) {
            return Response::error(
                400,
                format!(
                    "unknown table '{t}'; expected one of {}",
                    COUNTABLE.join(", ")
                ),
            );
        }
    }
    let by = query_param(query, "by");
    if let Some(b) = &by {
        if b != "origin_node" && b != "category" {
            return Response::error(
                400,
                format!("unknown grouping '{b}'; expected origin_node or category"),
            );
        }
    }
    let since = match query_param(query, "since") {
        Some(raw) => match canon_ts(&raw) {
            Ok(ts) => Some(ts),
            Err(_) => {
                return Response::error(
                    400,
                    format!("invalid since timestamp '{raw}'; expected ISO-8601"),
                )
            }
        },
        None => None,
    };
    let approx = query_flag(query, "approx");
    if approx && (since.is_some() || by.is_some()) {
        // A planner estimate is a whole-table row count; there is no filtered
        // or grouped equivalent. Rejecting beats silently ignoring the filter
        // and answering a different question than the one asked.
        return Response::error(
            400,
            "approx=1 cannot be combined with since or by; \
             planner estimates are whole-table only",
        );
    }

    let wanted: Vec<&str> = match &table {
        Some(t) => vec![t.as_str()],
        None => COUNTABLE.to_vec(),
    };

    // `approximate` reports what was actually served, not what was asked for:
    // a backend with no planner estimate falls back to exact counts, and
    // labelling those "approximate" would be the one thing the flag must never
    // mean.
    let (counts, approximate) = if approx {
        match store.approx_count_tables(&wanted) {
            Ok(Some(c)) => (Ok(c), true),
            Ok(None) => (store.count_tables(&wanted), false),
            Err(e) => (Err(e), false),
        }
    } else if let Some(since) = &since {
        (store.count_tables_since(&wanted, since), false)
    } else {
        (store.count_tables(&wanted), false)
    };

    let counts = match counts {
        Ok(c) => c,
        Err(e) => return storage_error("count", &e.0),
    };

    let by_origin = match by.as_deref() {
        Some("origin_node") => match store.count_by_origin_node(since.as_deref()) {
            Ok(rows) => Some(rows),
            Err(e) => return storage_error("count", &e.0),
        },
        _ => None,
    };
    // `by_category` is a top-level key here, not nested under `memories` the
    // way `/stats` reports it -- `RemoteCounts` (the `reconcile` client's
    // deserialisation target) expects it at the top level, and diverging
    // from that shape is exactly how `by_category` ends up silently always
    // empty, which is what made every `remind_me_sync_reconcile` call report
    // a false "pushes are not landing" verdict before this route supported
    // the grouping at all.
    let by_category = match by.as_deref() {
        Some("category") => match store.count_by_category(since.as_deref()) {
            Ok(rows) => Some(rows),
            Err(e) => return storage_error("count", &e.0),
        },
        _ => None,
    };

    let mut body = Map::new();
    body.insert("role".into(), json!("hub"));
    body.insert("version".into(), json!(HUB_VERSION));
    if let Some(since) = &since {
        body.insert("since".into(), json!(since));
    }
    if let Some(rows) = &by_origin {
        body.insert("by_origin_node".into(), pairs_to_object(rows));
    }
    if let Some(rows) = &by_category {
        body.insert("by_category".into(), pairs_to_object(rows));
    }
    // Always present, not only when true: a caller who forgot to ask would
    // otherwise have to infer exactness from the absence of a key.
    body.insert("approximate".into(), json!(approximate));
    insert_counts(&mut body, &counts);
    body.insert("time".into(), json!(now_canonical()));
    Response::json(200, &Value::Object(body))
}

/// Prometheus text exposition. 404 when disabled.
///
/// A disabled route returns 404 rather than 403, so "off" is indistinguishable
/// from "this build does not have it" — which is how a scrape config should
/// treat both anyway.
pub fn metrics(store: &dyn HubStore, config: &Config) -> Response {
    if !config.metrics_enabled {
        return Response::error(404, "metrics are not enabled");
    }
    let counts = match store.count_tables(&COUNTABLE) {
        Ok(c) => c,
        Err(e) => return storage_error("metrics", &e.0),
    };
    let memories = counts.memories.clone().unwrap_or_default();
    let body = format!(
        "# HELP remind_me_hub_build_info Build metadata; the value is always 1, \
         the labels carry the information.\n\
         # TYPE remind_me_hub_build_info gauge\n\
         remind_me_hub_build_info{{version=\"{version}\"}} 1\n\
         # HELP remind_me_hub_memories Memory records on the hub, by state.\n\
         # TYPE remind_me_hub_memories gauge\n\
         remind_me_hub_memories{{state=\"live\"}} {live}\n\
         remind_me_hub_memories{{state=\"tombstoned\"}} {tombstoned}\n\
         # HELP remind_me_hub_entities Entity records on the hub.\n\
         # TYPE remind_me_hub_entities gauge\n\
         remind_me_hub_entities {entities}\n\
         # HELP remind_me_hub_memory_entities Memory-entity link records on the hub.\n\
         # TYPE remind_me_hub_memory_entities gauge\n\
         remind_me_hub_memory_entities {links}\n\
         # HELP remind_me_hub_entity_relations Typed entity-to-entity edges on the hub.\n\
         # TYPE remind_me_hub_entity_relations gauge\n\
         remind_me_hub_entity_relations {relations}\n",
        version = HUB_VERSION,
        // Live and tombstoned are label values on one metric rather than two
        // metrics, so a dashboard can sum them for the total without knowing
        // both names.
        live = memories.live.unwrap_or(0),
        tombstoned = memories.tombstones.unwrap_or(0),
        entities = counts.entities.unwrap_or(0),
        links = counts.memory_entities.unwrap_or(0),
        relations = counts.entity_relations.unwrap_or(0),
    );
    Response::text(200, "text/plain; version=0.0.4", body)
}

/// Hard-delete memories tombstoned longer than the retention window ago.
///
/// Operator-triggered rather than an automatic background loop: the hub has no
/// periodic-task infrastructure to hang one off, and a cron hitting this is
/// both simpler and visible.
pub fn compact_tombstones(store: &dyn HubStore, config: &Config) -> Response {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(config.tombstone_retention_days);
    match store.compact_tombstones(&crate::canon::format_canonical(cutoff)) {
        Ok(purged) => {
            eprintln!(
                "hub: compacted {purged} tombstoned memories older than {} days",
                config.tombstone_retention_days
            );
            Response::json(
                200,
                &json!({
                    "purged": purged,
                    "retention_days": config.tombstone_retention_days,
                }),
            )
        }
        Err(e) => storage_error("compact_tombstones", &e.0),
    }
}

/// Upsert a batch, dispatching per record and isolating each one.
///
/// The three-number response is load-bearing and the distinctions are real:
/// `accepted` counts records that *changed something*, `processed_ids` is
/// every record the sender may retire from its outbox (including LWW losses,
/// which are settled, not pending), and `failed` counts records that were
/// malformed. A record that lost LWW is neither accepted nor failed.
pub fn push(store: &dyn HubStore, body: &[u8]) -> Response {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Response::error(400, "invalid push payload"),
    };
    let Some(records) = parsed.get("records").and_then(Value::as_array) else {
        return Response::error(400, "invalid push payload");
    };
    if records.len() > MAX_PUSH_BATCH {
        return Response::error(
            413,
            format!(
                "push batch of {} records exceeds the {MAX_PUSH_BATCH} limit",
                records.len()
            ),
        );
    }

    let origin = parsed
        .get("node_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut accepted = 0usize;
    let mut failed = 0usize;
    let mut processed_ids: Vec<String> = Vec::new();

    for raw in records {
        let record = match record::parse(raw) {
            Ok(r) => r,
            Err(e) => {
                failed += 1;
                eprintln!("hub: skipping malformed sync record: {e}");
                continue;
            }
        };
        match store.apply_record(&record, origin.as_deref()) {
            Ok(applied) => {
                processed_ids.push(record.wire_id());
                if applied {
                    accepted += 1;
                }
            }
            Err(e) => {
                // A storage failure is counted as failed and *not* added to
                // processed_ids, so the sender retries it. That is the whole
                // point of the split: malformed records are retired, transient
                // storage failures are not.
                failed += 1;
                eprintln!("hub: storage error applying record: {e}");
            }
        }
    }

    eprintln!(
        "hub: push from {}: {} records, {accepted} applied, {failed} failed",
        origin.as_deref().unwrap_or("unknown"),
        records.len()
    );
    Response::json(
        200,
        &json!({
            "accepted": accepted,
            "processed_ids": processed_ids,
            "failed": failed,
        }),
    )
}

/// Parse the cursor a `/sync/pull` request asked for.
///
/// Preference order matters: `since_seq` beats `since_id` beats bare `since`.
/// The sequence cursor is immune to the problem the timestamp cursors have —
/// a node back online after two weeks pushes records still stamped with old
/// timestamps, which sort *behind* an already-advanced `updated_at` cursor and
/// are then permanently invisible to every other node.
fn pull_query(query: &str) -> PullQuery {
    let since = query_param(query, "since").unwrap_or_else(|| EPOCH.to_string());
    let cursor = match query_param(query, "since_seq").and_then(|v| v.trim().parse::<i64>().ok()) {
        Some(seq) => PullCursor::Seq(seq),
        None => match query_param(query, "since_id") {
            Some(since_id) => PullCursor::Keyset { since, since_id },
            None => PullCursor::Since(since),
        },
    };
    PullQuery {
        cursor,
        exclude_node: query_param(query, "exclude_node").filter(|s| !s.is_empty()),
        full: query_flag(query, "full"),
        limit: limit_param(query),
    }
}

fn limit_param(query: &str) -> usize {
    clamp_limit(
        query_param(query, "limit")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(MAX_PULL_LIMIT),
    )
}

fn graph_pull_query(query: &str) -> GraphPullQuery {
    GraphPullQuery {
        since: query_param(query, "since").unwrap_or_else(|| EPOCH.to_string()),
        since_id: query_param(query, "since_id").unwrap_or_default(),
        limit: limit_param(query),
    }
}

pub fn pull(store: &dyn HubStore, query: &str) -> Response {
    respond_records(store.pull_memories(&pull_query(query)), "pull")
}

pub fn pull_entities(store: &dyn HubStore, query: &str) -> Response {
    respond_records(store.pull_entities(&pull_query(query)), "pull_entities")
}

pub fn pull_links(store: &dyn HubStore, query: &str) -> Response {
    respond_records(store.pull_links(&graph_pull_query(query)), "pull_links")
}

pub fn pull_entity_relations(store: &dyn HubStore, query: &str) -> Response {
    respond_records(
        store.pull_entity_relations(&graph_pull_query(query)),
        "pull_entity_relations",
    )
}

fn respond_records(result: crate::store::StoreResult<Vec<Value>>, route: &str) -> Response {
    match result {
        Ok(records) => Response::json(200, &json!({ "count": records.len(), "records": records })),
        Err(e) => storage_error(route, &e.0),
    }
}

/// A 500 that logs the detail and does not return it.
///
/// Same reasoning as `/health`'s `db` field: a storage error can embed host,
/// credentials and schema detail, and these routes are reachable through a
/// tunnel. The operator gets the full text in the log.
fn storage_error(route: &str, detail: &str) -> Response {
    eprintln!("hub: {route} failed: {detail}");
    Response::error(500, "internal error")
}

fn pairs_to_object(pairs: &[(String, i64)]) -> Value {
    let mut map = Map::new();
    for (key, count) in pairs {
        map.insert(key.clone(), json!(count));
    }
    Value::Object(map)
}

fn insert_counts(body: &mut Map<String, Value>, counts: &Counts) {
    if let Some(m) = &counts.memories {
        let mut entry = Map::new();
        entry.insert("total".into(), json!(m.total));
        // `live` and `tombstones` are present only when they were actually
        // computed. An approximate or since-filtered count has no honest split
        // to report, and inventing one is what the flag exists to prevent.
        if let Some(live) = m.live {
            entry.insert("live".into(), json!(live));
        }
        if let Some(tombstones) = m.tombstones {
            entry.insert("tombstones".into(), json!(tombstones));
        }
        body.insert("memories".into(), Value::Object(entry));
    }
    if let Some(n) = counts.entities {
        body.insert("entities".into(), json!(n));
    }
    if let Some(n) = counts.memory_entities {
        body.insert("memory_entities".into(), json!(n));
    }
    if let Some(n) = counts.entity_relations {
        body.insert("entity_relations".into(), json!(n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_seq_wins_over_since_id_and_since() {
        let q = pull_query("since=2026-01-01T00:00:00%2B00:00&since_id=abc&since_seq=42");
        assert_eq!(q.cursor, PullCursor::Seq(42));
    }

    #[test]
    fn since_id_wins_over_bare_since() {
        let q = pull_query("since=2026-01-01T00:00:00%2B00:00&since_id=abc");
        assert_eq!(
            q.cursor,
            PullCursor::Keyset {
                since: "2026-01-01T00:00:00+00:00".into(),
                since_id: "abc".into()
            }
        );
    }

    #[test]
    fn an_absent_cursor_defaults_to_the_epoch() {
        assert_eq!(pull_query("").cursor, PullCursor::Since(EPOCH.to_string()));
    }

    #[test]
    fn a_malformed_since_seq_falls_through_rather_than_erroring() {
        // Falling through to the timestamp cursor re-sends some records;
        // treating it as seq 0 would re-send everything, and erroring would
        // break a client over a typo in an optional optimisation.
        assert_eq!(
            pull_query("since_seq=banana").cursor,
            PullCursor::Since(EPOCH.to_string())
        );
    }

    #[test]
    fn the_limit_is_clamped_at_both_ends() {
        assert_eq!(limit_param("limit=0"), 1);
        assert_eq!(limit_param("limit=9999"), MAX_PULL_LIMIT);
        assert_eq!(limit_param(""), MAX_PULL_LIMIT);
        assert_eq!(limit_param("limit=7"), 7);
    }

    #[test]
    fn an_empty_exclude_node_is_treated_as_absent() {
        // `exclude_node=` would otherwise filter on the empty string and
        // silently drop every record whose origin_node is ''.
        assert_eq!(pull_query("exclude_node=").exclude_node, None);
    }
}
