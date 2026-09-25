//! A differential check: one script of pushes, applied to every backend
//! given, must leave them answering every read identically.
//!
//! Shared by `hub_multimodal_test.rs` (SQLite against the embedded engine,
//! no server needed) and `hub_postgres_test.rs` (all backends built in).
//! The script leans on the places backends can drift apart: ids that
//! share a timestamp and prefix each other, uppercase before lowercase in
//! byte order, an id at the 64-byte cap, LWW wins and losses, a win that
//! must keep the first `created_at`, tombstones, and cursors walked a page
//! at a time as a node walks them.
//!
//! It leaves out the one write whose result depends on the clock: an
//! entity enrichment that loses LWW stamps `now`. The route suite covers
//! that per backend.

use remind_me_hub::record;
use remind_me_hub::store::{GraphPullQuery, HubStore, PullCursor, PullQuery, COUNTABLE};
use serde_json::{json, Value};

fn memory(id: &str, created: &str, updated: &str) -> Value {
    json!({
        "id": id,
        "content": format!("content of {id}"),
        "category": if id.len().is_multiple_of(2) { "work" } else { "" },
        "tags": ["a", id],
        "metadata": { "id": id },
        "created_at": created,
        "updated_at": updated,
        "decay_rate": 0.25,
        "sensitive": id.starts_with('m'),
    })
}

fn entity(id: &str, kind: Option<&str>, aliases: Value, updated: &str) -> Value {
    json!({
        "record_type": "entity",
        "id": id,
        "name": format!("name of {id}"),
        "kind": kind,
        "aliases": aliases,
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": updated,
    })
}

fn link(memory_id: &str, entity_id: &str, created: &str) -> Value {
    json!({
        "record_type": "memory_entity",
        "memory_id": memory_id,
        "entity_id": entity_id,
        "created_at": created,
    })
}

fn relation(id: &str, created: &str) -> Value {
    json!({
        "record_type": "entity_relation",
        "id": id,
        "subject_entity_id": "e1",
        "relation": "knows",
        "object_entity_id": "e2",
        "created_at": created,
    })
}

/// The pushes, each with the node that pushed it.
fn script() -> Vec<(Value, &'static str)> {
    let t0 = "2026-08-05T10:00:00Z";
    let at_cap = "c".repeat(64);
    vec![
        // Three ids at one timestamp: "m" prefixes "m1", "Z" sorts first.
        (memory("m1", "2026-08-01T00:00:00Z", t0), "node-a"),
        (memory("m", "2026-08-01T00:00:00Z", t0), "node-b"),
        (memory("Z", "2026-08-01T00:00:00Z", t0), "node-a"),
        (
            memory(&at_cap, "2026-08-01T00:00:00Z", "2026-08-05T10:00:00.5Z"),
            "node-c",
        ),
        (
            memory("m2", "2026-08-01T00:00:00Z", "2026-08-05T11:00:00Z"),
            "node-b",
        ),
        // An LWW loser, then a winner carrying a different created_at the
        // stores must not take.
        (
            memory("m1", "2026-08-01T00:00:00Z", "2026-08-05T09:00:00Z"),
            "node-b",
        ),
        (
            memory("m2", "2026-08-03T00:00:00Z", "2026-08-06T00:00:00Z"),
            "node-a",
        ),
        // Unattributed, and a tombstone old enough to compact.
        (
            memory("u1", "2026-08-01T00:00:00Z", "2026-08-05T12:00:00Z"),
            "",
        ),
        (
            json!({
                "id": "gone",
                "content": "x",
                "created_at": "2026-08-01T00:00:00Z",
                "updated_at": "2026-08-02T00:00:00Z",
                "deleted_at": "2026-08-02T00:00:00Z",
            }),
            "node-a",
        ),
        // Entities: an insert, an LWW win that keeps the local kind and
        // unions aliases, and a loser that changes nothing.
        (entity("e1", Some("person"), json!(["Ada"]), t0), "node-a"),
        (
            entity("e1", None, json!(["Lovelace"]), "2026-08-06T00:00:00Z"),
            "node-b",
        ),
        (entity("e1", Some("person"), json!(["Ada"]), t0), "node-b"),
        (entity("E2", None, json!([]), t0), "node-a"),
        // Links share a timestamp and order on `memory_id|entity_id`; one
        // names a memory the hub does not hold, and one repeats.
        (link("m1", "e1", t0), "node-a"),
        (link("m", "e1", t0), "node-a"),
        (link("gone", "e1", t0), "node-a"),
        (link("nope", "e2", "2026-08-05T09:00:00Z"), "node-a"),
        (link("m1", "e1", t0), "node-b"),
        (relation("r2", t0), "node-a"),
        (relation("r1", t0), "node-a"),
        (relation("r1", "2026-08-09T00:00:00Z"), "node-b"),
    ]
}

/// More rows than the script can produce: a walk that passes this is a
/// cursor that never advances, and fails here rather than running forever.
const WALK_LIMIT: usize = 200;

fn extend_walk(out: &mut Vec<Value>, page: Vec<Value>, what: &str) {
    out.extend(page);
    assert!(
        out.len() <= WALK_LIMIT,
        "the {what} cursor walk never ended: the cursor does not advance past a page"
    );
}

fn origin(node: &str) -> Option<&str> {
    Some(node).filter(|n| !n.is_empty())
}

fn pull(
    store: &dyn HubStore,
    cursor: PullCursor,
    exclude: Option<&str>,
    limit: usize,
) -> Vec<Value> {
    store
        .pull_memories(&PullQuery {
            cursor,
            exclude_node: exclude.map(str::to_string),
            full: false,
            limit,
        })
        .expect("pull memories")
}

/// Every memory, walked `limit` at a time on the `hub_seq` cursor, as a
/// node on `since_seq` walks it.
fn walk_seq(store: &dyn HubStore, exclude: Option<&str>, limit: usize) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    loop {
        let seq = out.last().map_or(0, |m| m["hub_seq"].as_i64().unwrap());
        let page = pull(store, PullCursor::Seq(seq), exclude, limit);
        if page.is_empty() {
            return out;
        }
        extend_walk(&mut out, page, "hub_seq");
    }
}

/// Every memory, walked on the legacy `(updated_at, id)` keyset.
fn walk_keyset(store: &dyn HubStore, limit: usize) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    loop {
        let cursor = match out.last() {
            None => PullCursor::Since(remind_me_hub::EPOCH.to_string()),
            Some(last) => PullCursor::Keyset {
                since: last["updated_at"].as_str().unwrap().to_string(),
                since_id: last["id"].as_str().unwrap().to_string(),
            },
        };
        let page = pull(store, cursor, None, limit);
        if page.is_empty() {
            return out;
        }
        extend_walk(&mut out, page, "(updated_at, id)");
    }
}

/// Every link or relation, walked on the `(created_at, id)` keyset.
fn walk_graph(
    store: &dyn HubStore,
    pull: fn(&dyn HubStore, &GraphPullQuery) -> Vec<Value>,
    limit: usize,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    loop {
        let (since, since_id) = match out.last() {
            None => (remind_me_hub::EPOCH.to_string(), String::new()),
            Some(last) => (
                last["created_at"].as_str().unwrap().to_string(),
                last["id"].as_str().unwrap().to_string(),
            ),
        };
        let page = pull(
            store,
            &GraphPullQuery {
                since,
                since_id,
                limit,
            },
        );
        if page.is_empty() {
            return out;
        }
        extend_walk(&mut out, page, "(created_at, id)");
    }
}

fn links(store: &dyn HubStore, q: &GraphPullQuery) -> Vec<Value> {
    store.pull_links(q).expect("pull links")
}

fn relations(store: &dyn HubStore, q: &GraphPullQuery) -> Vec<Value> {
    store.pull_entity_relations(q).expect("pull relations")
}

/// How to compare `hub_seq` across backends.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SeqComparison {
    /// The same numbers: SQLite and the embedded engine both issue one per
    /// applied memory write, with no gaps.
    Exact,
    /// The same order. Postgres's `nextval()` also burns a number on every
    /// LWW loss, so its sequence has gaps the others do not. A node only
    /// relies on the order (it resumes strictly after its last `hub_seq`),
    /// so each value is replaced by its rank before comparing.
    Ranked,
}

/// Replace every memory's `hub_seq` in `observed` by its rank among the
/// store's `hub_seq`s.
fn rank_seqs(observed: &mut Value) {
    let mut seqs: Vec<i64> = observed["seq_by_1"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["hub_seq"].as_i64().unwrap())
        .collect();
    seqs.sort_unstable();
    for (_, list) in observed.as_object_mut().unwrap() {
        let Some(rows) = list.as_array_mut() else {
            continue;
        };
        for row in rows {
            if let Some(seq) = row.get("hub_seq").and_then(Value::as_i64) {
                row["hub_seq"] = json!(seqs.binary_search(&seq).expect("a seq the walk saw"));
            }
        }
    }
}

/// Everything a client can read, in one comparable value.
fn observe(store: &dyn HubStore) -> Value {
    let entity_query = |cursor, exclude: Option<&str>| PullQuery {
        cursor,
        exclude_node: exclude.map(str::to_string),
        full: false,
        limit: 500,
    };
    let stats = store.stats().expect("stats");
    let counts = store.count_tables(&COUNTABLE).expect("count");
    let since = "2026-08-05T10:00:00+00:00";
    let since_counts = store
        .count_tables_since(&COUNTABLE, since)
        .expect("count since");
    json!({
        "seq_by_1": walk_seq(store, None, 1),
        "seq_by_3_excluding_a": walk_seq(store, Some("node-a"), 3),
        "keyset_by_2": walk_keyset(store, 2),
        "since": pull(store, PullCursor::Since(since.to_string()), None, 500),
        "keyset_mid": pull(
            store,
            PullCursor::Keyset { since: since.to_string(), since_id: "m".to_string() },
            None,
            500,
        ),
        "entities": store.pull_entities(&entity_query(
            PullCursor::Since(remind_me_hub::EPOCH.to_string()), None)).unwrap(),
        "entities_seq_excluding_b": store.pull_entities(&entity_query(
            PullCursor::Seq(7), Some("node-b"))).unwrap(),
        "links_by_1": walk_graph(store, links, 1),
        "relations_by_1": walk_graph(store, relations, 1),
        "stats": format!("{stats:?}"),
        "counts": format!("{counts:?}"),
        "counts_since": format!("{since_counts:?}"),
        "by_origin": store.count_by_origin_node(None).unwrap(),
        "by_origin_since": store.count_by_origin_node(Some(since)).unwrap(),
        "by_category": store.count_by_category(None).unwrap(),
        "by_category_since": store.count_by_category(Some(since)).unwrap(),
    })
}

/// Apply the script to every backend, then require identical answers
/// from all of them to every read, before and after a tombstone
/// compaction. `backends` names each store for the failure message.
pub fn assert_backends_agree(backends: &[(&str, &dyn HubStore)], seq: SeqComparison) {
    let (first_name, first) = backends[0];
    for (raw, node) in script() {
        let parsed = record::parse(&raw).expect("a well-formed record");
        let expected = first
            .apply_record(&parsed, origin(node))
            .unwrap_or_else(|e| panic!("{first_name} apply {raw}: {e}"));
        for (name, store) in &backends[1..] {
            let applied = store
                .apply_record(&parsed, origin(node))
                .unwrap_or_else(|e| panic!("{name} apply {raw}: {e}"));
            assert_eq!(
                applied, expected,
                "{name} and {first_name} disagreed on whether {raw} applied"
            );
        }
    }
    assert_answers_agree(backends, seq, "after the pushes");

    let cutoff = "2026-08-04T00:00:00+00:00";
    let expected = first.compact_tombstones(cutoff).expect("compact");
    assert_eq!(expected, 1, "the script holds one expired tombstone");
    for (name, store) in &backends[1..] {
        assert_eq!(
            store.compact_tombstones(cutoff).expect("compact"),
            expected,
            "{name}"
        );
    }
    assert_answers_agree(backends, seq, "after compacting tombstones");
}

/// Push the script to one store, for a test that then copies it.
pub fn apply_script(store: &dyn HubStore) {
    for (raw, node) in script() {
        let parsed = record::parse(&raw).expect("a well-formed record");
        store
            .apply_record(&parsed, origin(node))
            .unwrap_or_else(|e| panic!("apply {raw}: {e}"));
    }
}

/// Require every read to answer alike on every backend, as they stand.
pub fn assert_answers_agree(backends: &[(&str, &dyn HubStore)], seq: SeqComparison, stage: &str) {
    let observe = |store: &dyn HubStore| {
        let mut observed = observe(store);
        if seq == SeqComparison::Ranked {
            rank_seqs(&mut observed);
        }
        observed
    };
    let (first_name, first) = backends[0];
    let expected = observe(first);
    for (name, store) in &backends[1..] {
        let got = observe(*store);
        for (key, want) in expected.as_object().unwrap() {
            assert_eq!(
                &got[key], want,
                "{stage}: {name} and {first_name} disagreed on {key}"
            );
        }
    }
}
