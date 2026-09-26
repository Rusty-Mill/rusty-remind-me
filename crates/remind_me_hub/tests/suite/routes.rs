//! The route suite, run by `tests/hub_routes_test.rs` against the store
//! `super::store()` gives it.
//!
//! Everything here is about the *protocol*: what a node sees on the wire.

use remind_me_hub::http::Head;
use remind_me_hub::store::HubStore;
use remind_me_hub::{dispatch, Config};
use serde_json::{json, Value};

use super::store;

const SECRET: &str = "test-secret";

fn config() -> Config {
    Config {
        secret: SECRET.to_string(),
        metrics_enabled: true,
        tombstone_retention_days: 90,
    }
}

fn head(method: &str, path: &str, query: &str, auth: bool) -> Head {
    Head {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        authorization: if auth {
            format!("Bearer {SECRET}")
        } else {
            String::new()
        },
        content_length: None,
    }
}

/// Send a request and return (status, parsed JSON body).
fn call(store: &dyn HubStore, method: &str, path: &str, query: &str, body: &Value) -> (u16, Value) {
    let raw = serde_json::to_vec(body).unwrap();
    let response = dispatch(store, &config(), &head(method, path, query, true), &raw);
    let parsed = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    (response.status, parsed)
}

fn get(store: &dyn HubStore, path: &str, query: &str) -> (u16, Value) {
    call(store, "GET", path, query, &json!(null))
}

fn memory(id: &str, updated: &str) -> Value {
    json!({
        "id": id,
        "content": format!("content of {id}"),
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": updated,
    })
}

fn push(store: &dyn HubStore, node: &str, records: Vec<Value>) -> Value {
    let (status, body) = call(
        store,
        "POST",
        "/sync/push",
        "",
        &json!({ "node_id": node, "records": records }),
    );
    assert_eq!(status, 200, "push should succeed: {body}");
    body
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[test]
fn every_route_but_health_requires_the_bearer() {
    let store = store();
    let cfg = config();
    for (method, path) in [
        ("GET", "/stats"),
        ("GET", "/count"),
        ("GET", "/metrics"),
        ("POST", "/admin/compact_tombstones"),
        ("POST", "/sync/push"),
        ("GET", "/sync/pull"),
        ("GET", "/sync/pull_entities"),
        ("GET", "/sync/pull_links"),
        ("GET", "/sync/pull_entity_relations"),
    ] {
        let response = dispatch(&store, &cfg, &head(method, path, "", false), b"");
        assert_eq!(response.status, 401, "{path} should require auth");
    }
}

#[test]
fn health_answers_without_a_bearer() {
    let store = store();
    let response = dispatch(&store, &config(), &head("GET", "/health", "", false), b"");
    assert_eq!(response.status, 200);
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["role"], "hub");
    assert_eq!(body["db"], "ok");
}

#[test]
fn a_known_path_with_the_wrong_method_is_405_not_404() {
    // Clients probe for a 404 to detect whether an endpoint exists, so a
    // wrong-verb request must not look like a missing capability.
    let store = store();
    let response = dispatch(&store, &config(), &head("GET", "/sync/push", "", true), b"");
    assert_eq!(response.status, 405);

    let response = dispatch(&store, &config(), &head("GET", "/nope", "", true), b"");
    assert_eq!(response.status, 404);
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

#[test]
fn a_pushed_memory_comes_back_from_pull() {
    let store = store();
    let result = push(&store, "node-a", vec![memory("m1", "2026-08-05T10:00:00Z")]);
    assert_eq!(result["accepted"], 1);
    assert_eq!(result["failed"], 0);
    assert_eq!(result["processed_ids"], json!(["m1"]));

    let (status, body) = get(&store, "/sync/pull", "");
    assert_eq!(status, 200);
    assert_eq!(body["count"], 1);
    assert_eq!(body["records"][0]["id"], "m1");
    // Canonicalised on the way in, not echoed as sent.
    assert_eq!(
        body["records"][0]["updated_at"],
        "2026-08-05T10:00:00+00:00"
    );
}

#[test]
fn a_pushed_memorys_sensitive_flag_comes_back_from_pull() {
    // #265: the hub's schema, wire columns and internal record type all
    // omitted `sensitive` entirely, so it was silently dropped on push and
    // every pulled memory defaulted to not-sensitive regardless of what was
    // pushed. This is the store-level round trip, closest to the bug itself
    // -- a_sensitive_memory_stays_sensitive_after_a_real_hub_round_trip in
    // remind_me_core covers the same fix one layer up, over a real socket.
    let store = store();
    let mut record = memory("m1", "2026-08-05T10:00:00Z");
    record["sensitive"] = json!(true);
    let result = push(&store, "node-a", vec![record]);
    assert_eq!(result["accepted"], 1);

    let (status, body) = get(&store, "/sync/pull", "");
    assert_eq!(status, 200);
    assert_eq!(
        body["records"][0]["sensitive"], true,
        "pulled record: {body}"
    );
}

#[test]
fn a_pushed_memory_with_no_sensitive_field_pulls_back_as_not_sensitive() {
    // The absent-key case, not just the true case -- an older client that
    // never sends `sensitive` at all must not be treated as an error, and
    // must not silently become `true` either.
    let store = store();
    let result = push(&store, "node-a", vec![memory("m1", "2026-08-05T10:00:00Z")]);
    assert_eq!(result["accepted"], 1);

    let (_, body) = get(&store, "/sync/pull", "");
    assert_eq!(body["records"][0]["sensitive"], false, "{body}");
}

#[test]
fn an_older_record_loses_last_write_wins_without_being_a_failure() {
    // The three-way distinction that makes the push response meaningful: an
    // LWW loss is settled business, so it is neither accepted nor failed, and
    // the sender may still retire it.
    let store = store();
    push(&store, "node-a", vec![memory("m1", "2026-08-05T12:00:00Z")]);
    let result = push(&store, "node-b", vec![memory("m1", "2026-08-05T10:00:00Z")]);

    assert_eq!(result["accepted"], 0, "an older record must not apply");
    assert_eq!(result["failed"], 0, "losing LWW is not a failure");
    assert_eq!(
        result["processed_ids"],
        json!(["m1"]),
        "a settled record must still be retired by the sender"
    );

    let (_, body) = get(&store, "/sync/pull", "");
    assert_eq!(
        body["records"][0]["updated_at"],
        "2026-08-05T12:00:00+00:00"
    );
}

#[test]
fn a_newer_record_wins_and_overwrites() {
    let store = store();
    push(&store, "node-a", vec![memory("m1", "2026-08-05T10:00:00Z")]);
    let result = push(&store, "node-b", vec![memory("m1", "2026-08-05T12:00:00Z")]);
    assert_eq!(result["accepted"], 1);

    let (_, body) = get(&store, "/sync/pull", "");
    assert_eq!(
        body["records"][0]["updated_at"],
        "2026-08-05T12:00:00+00:00"
    );
}

#[test]
fn one_malformed_record_does_not_poison_the_batch() {
    // The reason the reference wraps each record in its own savepoint.
    let store = store();
    let result = push(
        &store,
        "node-a",
        vec![
            memory("m1", "2026-08-05T10:00:00Z"),
            json!({ "id": "broken" }), // missing content/created_at/updated_at
            memory("m2", "2026-08-05T10:00:00Z"),
        ],
    );
    assert_eq!(result["accepted"], 2);
    assert_eq!(result["failed"], 1);
    assert_eq!(result["processed_ids"], json!(["m1", "m2"]));

    let (_, body) = get(&store, "/sync/pull", "");
    assert_eq!(body["count"], 2);
}

#[test]
fn a_batch_over_the_cap_is_refused_whole() {
    let store = store();
    let records: Vec<Value> = (0..1001)
        .map(|i| memory(&format!("m{i}"), "2026-08-05T10:00:00Z"))
        .collect();
    let (status, body) = call(
        &store,
        "POST",
        "/sync/push",
        "",
        &json!({ "node_id": "n", "records": records }),
    );
    assert_eq!(status, 413);
    assert!(body["detail"].as_str().unwrap().contains("1001"), "{body}");
}

#[test]
fn a_payload_without_a_records_array_is_rejected() {
    let store = store();
    for payload in [json!({}), json!({"records": "nope"}), json!([])] {
        let (status, _) = call(&store, "POST", "/sync/push", "", &payload);
        assert_eq!(status, 400, "payload {payload} should be rejected");
    }
}

// ---------------------------------------------------------------------------
// Pull cursors
// ---------------------------------------------------------------------------

#[test]
fn exclude_node_filters_on_the_pusher_not_the_authoring_node() {
    // The hub's one deliberate divergence from the peer server. A client never
    // rewrites node_id on update, so filtering on it would make a record's
    // creator deaf to every later edit -- here node-b edits node-a's record,
    // and node-a must still receive it.
    let store = store();
    let mut rec = memory("m1", "2026-08-05T10:00:00Z");
    rec["node_id"] = json!("node-a");
    push(&store, "node-b", vec![rec]);

    let (_, body) = get(&store, "/sync/pull", "exclude_node=node-a");
    assert_eq!(
        body["count"], 1,
        "the authoring node must still see an edit another node pushed"
    );

    let (_, body) = get(&store, "/sync/pull", "exclude_node=node-b");
    assert_eq!(body["count"], 0, "the pushing node should not get it back");
}

#[test]
fn full_overrides_exclude_node_so_a_wiped_node_can_reseed() {
    let store = store();
    push(&store, "node-a", vec![memory("m1", "2026-08-05T10:00:00Z")]);

    let (_, body) = get(&store, "/sync/pull", "exclude_node=node-a");
    assert_eq!(body["count"], 0);

    let (_, body) = get(&store, "/sync/pull", "exclude_node=node-a&full=1");
    assert_eq!(
        body["count"], 1,
        "full=1 must reach records the node itself pushed"
    );
}

#[test]
fn the_seq_cursor_sees_a_late_push_that_a_timestamp_cursor_cannot() {
    // The exact failure hub_seq exists for: a node offline for a fortnight
    // pushes records still stamped with old timestamps. They sort behind an
    // already-advanced updated_at cursor and are invisible forever.
    let store = store();
    push(
        &store,
        "node-a",
        vec![memory("recent", "2026-08-05T10:00:00Z")],
    );

    let (_, body) = get(&store, "/sync/pull", "");
    let watermark = body["records"][0]["updated_at"]
        .as_str()
        .unwrap()
        .to_string();
    let seq = body["records"][0]["hub_seq"].as_i64().unwrap();

    // The straggler: authored long ago, pushed now.
    push(
        &store,
        "node-b",
        vec![memory("stale", "2026-07-01T09:00:00Z")],
    );

    let (_, by_time) = get(
        &store,
        "/sync/pull",
        &format!("since={}", urlencode(&watermark)),
    );
    assert_eq!(
        by_time["count"], 0,
        "the timestamp cursor cannot see the straggler -- this is the bug"
    );

    let (_, by_seq) = get(&store, "/sync/pull", &format!("since_seq={seq}"));
    assert_eq!(by_seq["count"], 1, "the seq cursor must see it");
    assert_eq!(by_seq["records"][0]["id"], "stale");
}

#[test]
fn the_keyset_cursor_does_not_skip_records_sharing_a_timestamp() {
    // Why (updated_at, id) exists rather than a bare updated_at: three records
    // written in the same instant must all be reachable across pages.
    let store = store();
    push(
        &store,
        "node-a",
        vec![
            memory("a", "2026-08-05T10:00:00Z"),
            memory("b", "2026-08-05T10:00:00Z"),
            memory("c", "2026-08-05T10:00:00Z"),
        ],
    );

    let ts = urlencode("2026-08-05T10:00:00+00:00");
    let (_, page) = get(
        &store,
        "/sync/pull",
        &format!("since={ts}&since_id=a&limit=1"),
    );
    assert_eq!(page["count"], 1);
    assert_eq!(page["records"][0]["id"], "b");

    let (_, page) = get(
        &store,
        "/sync/pull",
        &format!("since={ts}&since_id=b&limit=1"),
    );
    assert_eq!(page["records"][0]["id"], "c");

    // The bare-since cursor is the one that skips them, which is exactly why
    // the keyset mode exists.
    let (_, skipped) = get(&store, "/sync/pull", &format!("since={ts}"));
    assert_eq!(skipped["count"], 0);
}

#[test]
fn the_limit_is_capped_server_side() {
    let store = store();
    let records: Vec<Value> = (0..10)
        .map(|i| memory(&format!("m{i:03}"), "2026-08-05T10:00:00Z"))
        .collect();
    push(&store, "node-a", records);

    let (_, body) = get(&store, "/sync/pull", "limit=3");
    assert_eq!(body["count"], 3);

    let (_, body) = get(&store, "/sync/pull", "limit=100000");
    assert_eq!(body["count"], 10, "a huge limit is clamped, not honoured");
}

// ---------------------------------------------------------------------------
// Entities, links, relations
// ---------------------------------------------------------------------------

fn entity(id: &str, name: &str, aliases: Value, updated: &str) -> Value {
    json!({
        "record_type": "entity",
        "id": id,
        "name": name,
        "aliases": aliases,
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": updated,
    })
}

#[test]
fn entity_aliases_union_merge_even_when_the_record_loses_lww() {
    // Union is commutative and idempotent, so merging regardless of who wins
    // is what makes every node converge on the same alias set.
    let store = store();
    push(
        &store,
        "node-a",
        vec![entity("e1", "Ada", json!(["Ada"]), "2026-08-05T12:00:00Z")],
    );
    let result = push(
        &store,
        "node-b",
        vec![entity(
            "e1",
            "Ada L",
            json!(["Lovelace"]),
            "2026-08-05T10:00:00Z",
        )],
    );
    assert_eq!(result["accepted"], 1, "the enrichment still applies");

    let (_, body) = get(&store, "/sync/pull_entities", "");
    let aliases = body["records"][0]["aliases"].as_array().unwrap();
    assert!(aliases.contains(&json!("Ada")), "{aliases:?}");
    assert!(aliases.contains(&json!("Lovelace")), "{aliases:?}");
    // The LWW winner's name is kept; only the aliases merged.
    assert_eq!(body["records"][0]["name"], "Ada");
}

#[test]
fn an_lww_losing_enrichment_bumps_updated_at_so_nodes_past_the_cursor_see_it() {
    // The hub is pull-only, so without a bump a node whose cursor has already
    // passed this entity would never receive the merged aliases.
    let store = store();
    push(
        &store,
        "node-a",
        vec![entity("e1", "Ada", json!(["Ada"]), "2026-08-05T12:00:00Z")],
    );
    let (_, before) = get(&store, "/sync/pull_entities", "");
    let original = before["records"][0]["updated_at"]
        .as_str()
        .unwrap()
        .to_string();

    push(
        &store,
        "node-b",
        vec![entity(
            "e1",
            "Ada",
            json!(["Lovelace"]),
            "2026-08-05T10:00:00Z",
        )],
    );
    let (_, after) = get(&store, "/sync/pull_entities", "");
    let bumped = after["records"][0]["updated_at"].as_str().unwrap();
    assert!(
        bumped > original.as_str(),
        "{bumped} should be after {original}"
    );
}

#[test]
fn an_idempotent_reapply_does_not_bump_again_so_the_cycle_terminates() {
    // The safety property behind the bump above.
    let store = store();
    push(
        &store,
        "node-a",
        vec![entity("e1", "Ada", json!(["Ada"]), "2026-08-05T12:00:00Z")],
    );
    push(
        &store,
        "node-b",
        vec![entity(
            "e1",
            "Ada",
            json!(["Lovelace"]),
            "2026-08-05T10:00:00Z",
        )],
    );
    let (_, first) = get(&store, "/sync/pull_entities", "");
    let after_merge = first["records"][0]["updated_at"]
        .as_str()
        .unwrap()
        .to_string();

    let result = push(
        &store,
        "node-b",
        vec![entity(
            "e1",
            "Ada",
            json!(["Lovelace"]),
            "2026-08-05T10:00:00Z",
        )],
    );
    assert_eq!(result["accepted"], 0, "a no-op merge must not apply");

    let (_, second) = get(&store, "/sync/pull_entities", "");
    assert_eq!(
        second["records"][0]["updated_at"].as_str().unwrap(),
        after_merge,
        "re-pulling an unchanged merge must not bump again"
    );
}

#[test]
fn links_are_immutable_and_carry_a_synthetic_id() {
    let store = store();
    let link = json!({
        "record_type": "memory_entity",
        "memory_id": "m1",
        "entity_id": "e1",
        "created_at": "2026-08-05T10:00:00Z",
    });
    let first = push(&store, "node-a", vec![link.clone()]);
    assert_eq!(first["accepted"], 1);
    assert_eq!(first["processed_ids"], json!(["m1|e1"]));

    let again = push(&store, "node-a", vec![link]);
    assert_eq!(again["accepted"], 0, "insert-or-ignore, not an update");
    assert_eq!(again["failed"], 0);

    let (_, body) = get(&store, "/sync/pull_links", "");
    assert_eq!(body["count"], 1);
    assert_eq!(body["records"][0]["id"], "m1|e1");
    assert_eq!(body["records"][0]["record_type"], "memory_entity");
}

#[test]
fn entity_relations_round_trip_with_their_real_id() {
    let store = store();
    push(
        &store,
        "node-a",
        vec![json!({
            "record_type": "entity_relation",
            "id": "r1",
            "subject_entity_id": "e1",
            "relation": "knows",
            "object_entity_id": "e2",
            "created_at": "2026-08-05T10:00:00Z",
        })],
    );
    let (_, body) = get(&store, "/sync/pull_entity_relations", "");
    assert_eq!(body["count"], 1);
    assert_eq!(body["records"][0]["id"], "r1");
    assert_eq!(body["records"][0]["relation"], "knows");
    assert_eq!(body["records"][0]["record_type"], "entity_relation");
}

#[test]
fn pulled_records_never_leak_origin_node() {
    // origin_node is hub bookkeeping; the wire format must be unchanged.
    let store = store();
    push(&store, "node-a", vec![memory("m1", "2026-08-05T10:00:00Z")]);
    push(
        &store,
        "node-a",
        vec![entity("e1", "Ada", json!([]), "2026-08-05T10:00:00Z")],
    );

    for path in ["/sync/pull", "/sync/pull_entities"] {
        let (_, body) = get(&store, path, "");
        let record = &body["records"][0];
        assert!(
            record.get("origin_node").is_none(),
            "{path} leaked origin_node: {record}"
        );
    }
}

// ---------------------------------------------------------------------------
// Stats, count, metrics, compaction
// ---------------------------------------------------------------------------

#[test]
fn stats_reports_totals_and_the_hub_only_origin_breakdown() {
    let store = store();
    push(&store, "node-a", vec![memory("m1", "2026-08-05T10:00:00Z")]);
    push(&store, "node-b", vec![memory("m2", "2026-08-05T11:00:00Z")]);
    let mut deleted = memory("m3", "2026-08-05T12:00:00Z");
    deleted["deleted_at"] = json!("2026-08-05T12:00:00Z");
    push(&store, "node-b", vec![deleted]);

    let (status, body) = get(&store, "/stats", "");
    assert_eq!(status, 200);
    assert_eq!(body["memories"]["total"], 3);
    assert_eq!(body["memories"]["tombstones"], 1);
    assert_eq!(body["memories"]["by_origin_node"]["node-a"], 1);
    assert_eq!(body["memories"]["by_origin_node"]["node-b"], 2);
    assert_eq!(
        body["memories"]["oldest_updated_at"],
        "2026-08-05T10:00:00+00:00"
    );
    assert_eq!(
        body["memories"]["newest_updated_at"],
        "2026-08-05T12:00:00+00:00"
    );
    assert_eq!(body["role"], "hub");
}

#[test]
fn count_splits_live_from_tombstoned() {
    let store = store();
    push(&store, "node-a", vec![memory("m1", "2026-08-05T10:00:00Z")]);
    let mut deleted = memory("m2", "2026-08-05T11:00:00Z");
    deleted["deleted_at"] = json!("2026-08-05T11:00:00Z");
    push(&store, "node-a", vec![deleted]);

    let (_, body) = get(&store, "/count", "");
    assert_eq!(body["memories"]["total"], 2);
    assert_eq!(body["memories"]["live"], 1);
    assert_eq!(body["memories"]["tombstones"], 1);
    assert_eq!(body["approximate"], false);
}

#[test]
fn count_reports_a_category_breakdown_when_asked() {
    // `RemoteCounts` (the `reconcile` client's deserialisation target)
    // expects `by_category` as a top-level key -- unlike `/stats`, which
    // nests it under `memories`. Regression for the reconcile bug where
    // `/count` never returned this key at all, so every reconcile call read
    // every remote category as 0 and reported a false `NodeAhead`.
    let store = store();
    let mut a = memory("m1", "2026-08-05T10:00:00Z");
    a["category"] = json!("fact");
    push(&store, "node-a", vec![a]);
    let mut b = memory("m2", "2026-08-05T11:00:00Z");
    b["category"] = json!("fact");
    push(&store, "node-a", vec![b]);
    let mut c = memory("m3", "2026-08-05T12:00:00Z");
    c["category"] = json!("preference");
    push(&store, "node-a", vec![c]);

    let (status, body) = get(&store, "/count", "by=category");
    assert_eq!(status, 200);
    assert_eq!(body["by_category"]["fact"], 2);
    assert_eq!(body["by_category"]["preference"], 1);

    // Without the flag, no key at all -- a caller who forgot to ask must not
    // be able to mistake absence for zero.
    let (_, body) = get(&store, "/count", "");
    assert!(body.get("by_category").is_none());
}

#[test]
fn count_rejects_an_unknown_table_and_grouping() {
    let store = store();
    let (status, body) = get(&store, "/count", "table=passwords");
    assert_eq!(status, 400);
    assert!(body["detail"].as_str().unwrap().contains("passwords"));

    let (status, _) = get(&store, "/count", "by=content");
    assert_eq!(status, 400);
}

#[test]
fn count_rejects_approx_combined_with_a_filter() {
    // The estimate is whole-table only; silently ignoring the filter would
    // answer a different question than the one asked.
    let store = store();
    let (status, _) = get(&store, "/count", "approx=1&by=origin_node");
    assert_eq!(status, 400);
    let (status, _) = get(&store, "/count", "approx=1&since=2026-01-01T00:00:00Z");
    assert_eq!(status, 400);
}

#[test]
fn count_reports_approximate_false_when_the_backend_has_no_estimate() {
    // The engine has no planner row count, so asking for approx must fall back
    // to exact and *say so* -- labelling a scan "approximate" is the one thing
    // the flag must never mean.
    let store = store();
    push(&store, "node-a", vec![memory("m1", "2026-08-05T10:00:00Z")]);
    let (status, body) = get(&store, "/count", "approx=1");
    assert_eq!(status, 200);
    assert_eq!(body["approximate"], false);
    assert_eq!(body["memories"]["total"], 1);
}

#[test]
fn count_since_filters_and_omits_the_live_split() {
    let store = store();
    push(
        &store,
        "node-a",
        vec![memory("old", "2026-08-01T10:00:00Z")],
    );
    push(
        &store,
        "node-a",
        vec![memory("new", "2026-08-05T10:00:00Z")],
    );

    let (_, body) = get(
        &store,
        "/count",
        &format!("since={}", urlencode("2026-08-03T00:00:00+00:00")),
    );
    assert_eq!(body["memories"]["total"], 1);
    assert!(
        body["memories"].get("live").is_none(),
        "a since-filtered count has no honest live/tombstone split: {}",
        body["memories"]
    );
    assert_eq!(body["since"], "2026-08-03T00:00:00+00:00");
}

#[test]
fn count_rejects_a_malformed_since() {
    let store = store();
    let (status, body) = get(&store, "/count", "since=yesterday");
    assert_eq!(status, 400);
    assert!(body["detail"].as_str().unwrap().contains("yesterday"));
}

#[test]
fn metrics_are_prometheus_text_and_404_when_disabled() {
    let store = store();
    push(&store, "node-a", vec![memory("m1", "2026-08-05T10:00:00Z")]);

    let response = dispatch(&store, &config(), &head("GET", "/metrics", "", true), b"");
    assert_eq!(response.status, 200);
    assert_eq!(response.content_type, "text/plain; version=0.0.4");
    let text = String::from_utf8(response.body).unwrap();
    assert!(
        text.contains("remind_me_hub_memories{state=\"live\"} 1"),
        "{text}"
    );
    assert!(text.contains("remind_me_hub_build_info{version="), "{text}");

    let off = Config {
        metrics_enabled: false,
        ..config()
    };
    let response = dispatch(&store, &off, &head("GET", "/metrics", "", true), b"");
    assert_eq!(
        response.status, 404,
        "disabled must be indistinguishable from absent"
    );
}

#[test]
fn compaction_removes_expired_tombstones_and_their_links() {
    let store = store();
    let mut old = memory("old", "2026-01-01T00:00:00Z");
    old["deleted_at"] = json!("2026-01-01T00:00:00Z");
    let mut recent = memory("recent", "2026-08-05T00:00:00Z");
    recent["deleted_at"] = json!("2026-08-05T00:00:00Z");
    push(&store, "node-a", vec![old, recent]);
    push(
        &store,
        "node-a",
        vec![json!({
            "record_type": "memory_entity",
            "memory_id": "old",
            "entity_id": "e1",
            "created_at": "2026-01-01T00:00:00Z",
        })],
    );

    let (status, body) = call(
        &store,
        "POST",
        "/admin/compact_tombstones",
        "",
        &json!(null),
    );
    assert_eq!(status, 200);
    assert_eq!(body["purged"], 1, "only the expired tombstone");
    assert_eq!(body["retention_days"], 90);

    let (_, body) = get(&store, "/sync/pull", "");
    assert_eq!(body["count"], 1);
    assert_eq!(body["records"][0]["id"], "recent");

    let (_, links) = get(&store, "/sync/pull_links", "");
    assert_eq!(
        links["count"], 0,
        "the purged memory's links must go with it"
    );
}

#[test]
fn a_live_memory_is_never_compacted_however_old() {
    let store = store();
    push(
        &store,
        "node-a",
        vec![memory("ancient", "2020-01-01T00:00:00Z")],
    );
    let (_, body) = call(
        &store,
        "POST",
        "/admin/compact_tombstones",
        "",
        &json!(null),
    );
    assert_eq!(body["purged"], 0);
    let (_, body) = get(&store, "/sync/pull", "");
    assert_eq!(body["count"], 1);
}

#[test]
fn compacting_the_newest_tombstone_never_reissues_its_hub_seq() {
    // A node's since_seq cursor resumes strictly after the last hub_seq it
    // saw. If compaction purged the row holding the highest seq and the next
    // write reused that number, a node already at that cursor would never
    // pull the new row.
    let store = store();
    push(&store, "node-a", vec![memory("m1", "2026-08-01T00:00:00Z")]);
    let mut m2 = memory("m2", "2026-01-01T00:00:00Z");
    m2["deleted_at"] = json!("2026-01-01T00:00:00Z");
    push(&store, "node-a", vec![m2]);

    let (_, body) = get(&store, "/sync/pull", "since_seq=0");
    assert_eq!(body["records"][1]["id"], "m2");
    let cursor = body["records"][1]["hub_seq"].as_i64().unwrap();
    assert_eq!(cursor, 2);

    let (_, body) = call(
        &store,
        "POST",
        "/admin/compact_tombstones",
        "",
        &json!(null),
    );
    assert_eq!(body["purged"], 1, "m2 is the expired tombstone");

    push(&store, "node-a", vec![memory("m3", "2026-08-02T00:00:00Z")]);

    let (_, body) = get(&store, "/sync/pull", &format!("since_seq={cursor}"));
    assert_eq!(body["count"], 1, "a node at the old cursor must see m3");
    assert_eq!(body["records"][0]["id"], "m3");
    assert_eq!(body["records"][0]["hub_seq"], 3);
}

/// Minimal percent-encoding for the query values these tests build.
fn urlencode(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            ':' => "%3A".to_string(),
            '+' => "%2B".to_string(),
            other => other.to_string(),
        })
        .collect()
}
