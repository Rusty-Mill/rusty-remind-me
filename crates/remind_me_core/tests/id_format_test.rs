//! Memory ids are opaque, and both formats coexist in a shared store (#217).
//!
//! `remind_me` writes `sha256(content + timestamp)[:12]`; this crate writes
//! `mem_` plus a uuid4. Both land in the same `memories.id` column of the same
//! database, because Tenet 3 means the two implementations share a file.
//!
//! That interop was verified by hand against a live reference database and
//! then held *by accident* — nothing asserted it, and nothing recorded that two
//! formats were expected. `docs/adr/0016` records the decision; these are the
//! guards that keep it true.
//!
//! The thing being defended is a negative: no read, write, update or delete
//! path may parse, measure, or pattern-match an id. So these drive
//! reference-shaped ids through the real query paths rather than inspecting the
//! generator, because a generator test would keep passing if `get_memory_by_id`
//! started assuming a `mem_` prefix.

use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::testing;
use remind_me_core::{Database, MemoryAddInput, MemoryListInput, MemoryUpdateInput};

/// Ids in the reference's shape, as it would actually write them.
///
/// Not `mem_`-prefixed, exactly 12 lowercase hex characters. Taken from a real
/// `remind_me`-created database rather than invented.
const REFERENCE_SHAPED: [&str; 3] = ["b14392f2f0aa", "60a83dd9662f", "c38fed0e0bb5"];

fn add(store: &Store<'_>, content: &str) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.to_string(),
            category: "general".into(),
            tags: vec!["t".into()],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive: false,
        },
    )
    .unwrap()
    .id
}

/// Rewrite a row's id to a reference-shaped one, simulating a row this
/// implementation received from `remind_me` through the shared database.
///
/// Done with a raw write because there is no API for it — which is the
/// point: a row with a foreign id shape arrives by the other process writing
/// it, not by anything here choosing it. Its tags move with it, not dropped:
/// a row that arrived from `remind_me` has its tags, and a guard that only
/// ever sees tagless rows would not be testing the shape a shared database
/// holds.
fn relabel(store: &Store<'_>, from: &str, to: &str) {
    // Left inconsistent, the tests below would pass for the wrong reason:
    // the helper refuses a move that leaves tag rows behind.
    assert_eq!(testing::relabel_memory(store, from, to).unwrap(), 1);
}

/// `list` with a real limit.
///
/// `MemoryListInput` derives `Default`, so `..Default::default()` gives
/// `limit: 0` — the `#[serde(default = "default_list_limit")]` attribute only
/// applies when deserializing. A test that used the derived default would list
/// nothing and every assertion below would be vacuous.
fn list_all(store: &Store<'_>) -> Vec<String> {
    let input = MemoryListInput {
        limit: 100,
        ..Default::default()
    };
    queries::list_memories(store, &input)
        .unwrap()
        .memories
        .into_iter()
        .map(|m| m.id)
        .collect()
}

// ---------------------------------------------------------------------------
// A foreign id survives every path
// ---------------------------------------------------------------------------

#[test]
fn a_reference_shaped_id_reads_back() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    for id in REFERENCE_SHAPED {
        let ours = add(&store, &format!("row {id}"));
        relabel(&store, &ours, id);

        let found = queries::get_memory_by_id(&store, id)
            .unwrap()
            .unwrap_or_else(|| panic!("a {id:?} id must be readable"));
        assert_eq!(found.id, id);
        assert_eq!(found.content, format!("row {id}"));
    }
}

#[test]
fn a_reference_shaped_id_updates() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let ours = add(&store, "before");
    relabel(&store, &ours, REFERENCE_SHAPED[0]);

    queries::update_memory(
        &store,
        &MemoryUpdateInput {
            memory_id: REFERENCE_SHAPED[0].to_string(),
            clear_superseded: false,
            content: Some("after".into()),
            category: None,
            tags: None,
            metadata: None,
            sensitive: None,
        },
    )
    .expect("an update must not care what shape the id is");

    let found = queries::get_memory_by_id(&store, REFERENCE_SHAPED[0])
        .unwrap()
        .unwrap();
    assert_eq!(found.content, "after");
}

#[test]
fn a_reference_shaped_id_deletes() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let ours = add(&store, "doomed");
    relabel(&store, &ours, REFERENCE_SHAPED[1]);

    assert!(queries::delete_memory(&store, REFERENCE_SHAPED[1]).unwrap());
    assert!(queries::get_memory_by_id(&store, REFERENCE_SHAPED[1])
        .unwrap()
        .is_none());
}

#[test]
fn both_formats_coexist_and_list_together() {
    // The actual shared-database state: some rows written here, some written by
    // `remind_me`. A read path that filtered on shape would return half.
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    let ours = add(&store, "written here");
    let theirs_src = add(&store, "written by the reference");
    relabel(&store, &theirs_src, REFERENCE_SHAPED[2]);

    let ids = list_all(&store);

    assert!(ids.contains(&ours), "ours missing from {ids:?}");
    assert!(
        ids.iter().any(|i| i == REFERENCE_SHAPED[2]),
        "the reference-shaped row is missing from {ids:?}"
    );
    assert_eq!(ids.len(), 2);
}

// ---------------------------------------------------------------------------
// The prefix is not a contract
// ---------------------------------------------------------------------------

#[test]
fn nothing_in_the_crate_dispatches_on_the_mem_prefix() {
    // ADR-0016 point 1. If someone starts branching on `mem_`, an id without it
    // stops working — and in a shared store, half the rows have no prefix.
    //
    // A row with a *deliberately unlike* id: no prefix, not hex, not 12 chars.
    // If any path pattern-matched, this is what would break first.
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let ours = add(&store, "oddly named");
    relabel(&store, &ours, "not-a-mem-id-at-all-☃");

    let found = queries::get_memory_by_id(&store, "not-a-mem-id-at-all-☃")
        .unwrap()
        .expect("ids are opaque, so even this must round-trip");
    assert_eq!(found.content, "oddly named");
    assert!(queries::delete_memory(&store, "not-a-mem-id-at-all-☃").unwrap());
}

#[test]
fn our_own_ids_are_unique_rather_than_derived_from_content() {
    // ADR-0016's reason for *not* adopting the reference's scheme. Identical
    // content added twice must produce two rows, not a collision — the
    // reference's `sha256(content + ts)[:12]` is a function of exactly the
    // inputs a duplicate shares.
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    let a = add(&store, "exactly the same text");
    let b = add(&store, "exactly the same text");

    assert_ne!(a, b, "identical content must not collide on id");
    assert_eq!(list_all(&store).len(), 2, "both rows must survive");
}

// ---------------------------------------------------------------------------
// Where determinism IS the contract
// ---------------------------------------------------------------------------

#[test]
fn entity_ids_match_the_reference_byte_for_byte() {
    // Deliberately excluded from ADR-0016's "ids are opaque" rule. Entity ids
    // are content-addressed on purpose: the determinism is *how* two peers
    // agree on the same entity without coordinating, so a divergence here would
    // split one entity into two across a sync.
    //
    // These expected values were computed by running `remind_me`'s own
    // `_entity_id` (v1.54.0), not derived from this implementation.
    assert_eq!(
        remind_me_core::entity::entity_id("Bailey Robertson"),
        "494292a0dfb1"
    );
    // Normalisation has to agree too, or the same entity gets two ids.
    assert_eq!(
        remind_me_core::entity::entity_id("  ACME  Corp  "),
        "ea6f9c07a2f9"
    );
    assert_eq!(
        remind_me_core::entity::entity_id("ACME corp"),
        "ea6f9c07a2f9",
        "casing and inner whitespace must normalise to one id"
    );
}
