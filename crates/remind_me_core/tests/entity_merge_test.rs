//! Person-confirmed entity merge and its revert (card #567, pack D101-D107):
//! links, relations and names move to the survivor in one transaction, a
//! repeat is a no-op, the record restores the absorbed entity exactly, and a
//! later mention of the absorbed name lands on the survivor.

use remind_me_core::db::entities::{Entities, StoredRelation};
use remind_me_core::db::memories::Memories;
use remind_me_core::db::{queries, Store};
use remind_me_core::entity::{
    entity_id, get_entity_by_id, link_memory_entity, upsert_entity, upsert_entity_exact,
    upsert_entity_relation, Entity,
};
use remind_me_core::entity_merge::{self, Outcome, RevertOutcome, MERGED_INTO};
use remind_me_core::{Database, EntityInput, MemoryAddInput};
use std::collections::HashSet;

fn memory(store: &Store<'_>, content: &str) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.to_string(),
            category: "general".into(),
            source: "manual".into(),
            metadata: serde_json::json!({}),
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

fn entity(store: &Store<'_>, name: &str, kind: Option<&str>, aliases: &[&str]) -> Entity {
    upsert_entity(
        store,
        &EntityInput {
            name: name.to_string(),
            kind: kind.map(str::to_string),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
        },
    )
    .unwrap()
}

fn linked(store: &Store<'_>, entity: &str, memories: &[&str]) {
    for m in memories {
        link_memory_entity(store, m, entity).unwrap();
    }
}

fn links(store: &Store<'_>, id: &str) -> HashSet<String> {
    Memories::new(store).linked_ids(id).unwrap()
}

fn relations(store: &Store<'_>) -> Vec<StoredRelation> {
    let mut all = Entities::new(store).relations_oldest_first().unwrap();
    all.sort_by(|a, b| a.id.cmp(&b.id));
    all
}

fn merged(outcome: Outcome) -> (entity_merge::Plan, String) {
    match outcome {
        Outcome::Merged { plan, record_id } => (plan, record_id),
        other => panic!("expected a merge, got {other:?}"),
    }
}

/// A (`rusty_mux`) and B (`rusty-mux`) with links: m1, m2 on B only, m3 on
/// both, m4 on A only.
struct Pair {
    a: Entity,
    b: Entity,
    m: [String; 4],
}

fn pair(store: &Store<'_>) -> Pair {
    let a = entity(store, "rusty_mux", None, &["mux"]);
    let b = entity(store, "rusty-mux", Some("crate"), &["the mux server"]);
    let m = ["one", "two", "three", "four"].map(|c| memory(store, c));
    linked(store, &b.id, &[&m[0], &m[1], &m[2]]);
    linked(store, &a.id, &[&m[2], &m[3]]);
    Pair { a, b, m }
}

#[test]
fn preview_counts_and_writes_nothing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let p = pair(&store);
    let before = relations(&store);

    let Outcome::Preview { plan } =
        entity_merge::preview(&store, "rusty-mux", "rusty_mux").unwrap()
    else {
        panic!("expected a preview");
    };
    assert_eq!((plan.links_moved, plan.links_shared), (2, 1));
    assert_eq!(plan.from.links, 3);
    assert_eq!(plan.into.links, 2);
    assert_eq!(plan.aliases_gained, vec!["the mux server", "rusty-mux"]);
    assert_eq!(plan.kind_filled.as_deref(), Some("crate"));

    assert!(get_entity_by_id(&store, &p.b.id).unwrap().is_some());
    assert_eq!(links(&store, &p.a.id).len(), 2);
    assert_eq!(relations(&store), before);
    assert!(Memories::new(&store)
        .with_predicate_prefix(MERGED_INTO)
        .unwrap()
        .is_empty());
}

#[test]
fn merge_moves_links_and_names_and_removes_b() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let p = pair(&store);

    let (plan, record_id) = merged(entity_merge::merge(&store, "rusty-mux", "rusty_mux").unwrap());

    assert_eq!((plan.links_moved, plan.links_shared), (2, 1));
    assert!(get_entity_by_id(&store, &p.b.id).unwrap().is_none());
    assert!(links(&store, &p.b.id).is_empty());
    let want: HashSet<String> = p.m.iter().cloned().collect();
    assert_eq!(links(&store, &p.a.id), want);

    let a = get_entity_by_id(&store, &p.a.id).unwrap().unwrap();
    assert_eq!(a.aliases, vec!["mux", "the mux server", "rusty-mux"]);
    assert_eq!(a.kind.as_deref(), Some("crate"));
    assert_eq!(
        a.created_at,
        p.a.created_at.clone().min(p.b.created_at.clone())
    );

    let record = Memories::new(&store).get_live(&record_id).unwrap().unwrap();
    assert_eq!(record.predicate.as_deref(), Some(MERGED_INTO));
    assert_eq!(record.subject.as_deref(), Some("rusty-mux"));
    assert_eq!(record.object.as_deref(), Some("rusty_mux"));
    assert!(record.tags.contains(&"entity-merge".to_string()));
}

#[test]
fn a_kind_a_already_has_is_kept() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    entity(&store, "Attest", Some("tool"), &[]);
    entity(&store, "attest-store", Some("crate"), &[]);

    let (plan, _) = merged(entity_merge::merge(&store, "attest-store", "Attest").unwrap());

    assert_eq!(plan.kind_filled, None);
    let a = get_entity_by_id(&store, &entity_id("Attest"))
        .unwrap()
        .unwrap();
    assert_eq!(a.kind.as_deref(), Some("tool"));
}

#[test]
fn relations_move_both_ways_without_self_loops_or_duplicates() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let p = pair(&store);
    let c = entity(&store, "rusty_term", None, &[]);
    upsert_entity_relation(&store, &p.b.id, "uses", &c.id).unwrap(); // A has it: dropped
    upsert_entity_relation(&store, &p.a.id, "uses", &c.id).unwrap();
    upsert_entity_relation(&store, &c.id, "feeds", &p.b.id).unwrap(); // moved
    upsert_entity_relation(&store, &p.b.id, "same as", &p.a.id).unwrap(); // self-loop: dropped

    let (plan, _) = merged(entity_merge::merge(&store, "rusty-mux", "rusty_mux").unwrap());

    assert_eq!((plan.relations_moved, plan.relations_dropped), (1, 2));
    let triples: HashSet<(String, String, String)> = relations(&store)
        .into_iter()
        .map(|r| (r.subject_entity_id, r.relation, r.object_entity_id))
        .collect();
    let want: HashSet<(String, String, String)> = [
        (p.a.id.clone(), "uses".to_string(), c.id.clone()),
        (c.id.clone(), "feeds".to_string(), p.a.id.clone()),
    ]
    .into_iter()
    .collect();
    assert_eq!(triples, want);
    // The moved row sits under A's derived id, so a later upsert finds it.
    assert!(!upsert_entity_relation(&store, &c.id, "feeds", &p.a.id).unwrap());
}

#[test]
fn a_repeat_merge_is_a_no_op_naming_the_first() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    pair(&store);
    let (_, record_id) = merged(entity_merge::merge(&store, "rusty-mux", "rusty_mux").unwrap());

    let again = entity_merge::merge(&store, "rusty-mux", "rusty_mux").unwrap();

    assert_eq!(
        again,
        Outcome::AlreadyMerged {
            record_id,
            into: "rusty_mux".into()
        }
    );
    assert_eq!(
        Memories::new(&store)
            .with_predicate_prefix(MERGED_INTO)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn revert_restores_b_exactly() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let p = pair(&store);
    let c = entity(&store, "rusty_term", None, &[]);
    upsert_entity_relation(&store, &p.b.id, "uses", &c.id).unwrap();
    upsert_entity_relation(&store, &p.a.id, "uses", &c.id).unwrap();
    upsert_entity_relation(&store, &c.id, "feeds", &p.b.id).unwrap();
    upsert_entity_relation(&store, &p.b.id, "same as", &p.a.id).unwrap();
    let relations_before = relations(&store);
    let a_before = get_entity_by_id(&store, &p.a.id).unwrap().unwrap();
    let (a_links, b_links) = (links(&store, &p.a.id), links(&store, &p.b.id));
    let (_, record_id) = merged(entity_merge::merge(&store, "rusty-mux", "rusty_mux").unwrap());

    let outcome = entity_merge::revert(&store, &record_id, "rusty-mux").unwrap();

    let RevertOutcome::Reverted {
        revert_id,
        links_restored,
        ..
    } = outcome
    else {
        panic!("expected a revert, got {outcome:?}");
    };
    assert_eq!(links_restored, 3);
    let b = get_entity_by_id(&store, &p.b.id).unwrap().unwrap();
    assert_eq!(
        (b.name, b.kind, b.aliases, b.created_at),
        (
            p.b.name.clone(),
            p.b.kind.clone(),
            p.b.aliases.clone(),
            p.b.created_at.clone()
        )
    );
    let a = get_entity_by_id(&store, &p.a.id).unwrap().unwrap();
    assert_eq!(
        (a.kind, a.aliases, a.created_at),
        (a_before.kind, a_before.aliases, a_before.created_at)
    );
    assert_eq!(links(&store, &p.a.id), a_links);
    assert_eq!(links(&store, &p.b.id), b_links);
    let strip = |rs: Vec<StoredRelation>| -> Vec<(String, String, String, String)> {
        rs.into_iter()
            .map(|r| (r.id, r.subject_entity_id, r.relation, r.object_entity_id))
            .collect()
    };
    assert_eq!(strip(relations(&store)), strip(relations_before));

    let record = Memories::new(&store).get_live(&record_id).unwrap().unwrap();
    assert_eq!(record.superseded_by.as_deref(), Some(revert_id.as_str()));
    assert_eq!(
        entity_merge::revert(&store, &record_id, "rusty-mux").unwrap(),
        RevertOutcome::AlreadyReverted {
            record_id: record_id.clone(),
            revert_id
        }
    );
    // Reverted, so a fresh merge is allowed again.
    merged(entity_merge::merge(&store, "rusty-mux", "rusty_mux").unwrap());
}

#[test]
fn revert_keeps_what_a_gained_since_and_skips_deleted_memories() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let p = pair(&store);
    let (_, record_id) = merged(entity_merge::merge(&store, "rusty-mux", "rusty_mux").unwrap());
    let later = memory(&store, "five");
    link_memory_entity(&store, &later, &p.a.id).unwrap();
    queries::delete_memory(&store, &p.m[0]).unwrap();

    entity_merge::revert(&store, &record_id, "rusty-mux").unwrap();

    assert!(links(&store, &p.a.id).contains(&later));
    assert!(!links(&store, &p.b.id).contains(&p.m[0]));
}

#[test]
fn revert_names_the_absorbed_entity() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    pair(&store);
    let (_, record_id) = merged(entity_merge::merge(&store, "rusty-mux", "rusty_mux").unwrap());

    let err = entity_merge::revert(&store, &record_id, "rusty_term").unwrap_err();

    assert!(err.to_string().contains("not \"rusty_term\""), "{err}");
    let not_a_record = memory(&store, "plain");
    assert!(entity_merge::revert(&store, &not_a_record, "rusty-mux").is_err());
}

#[test]
fn refuses_self_topic_and_project_merges() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    entity(&store, "rusty_mux", None, &["mux"]);
    entity(&store, "rusty_term", Some("project"), &[]);
    upsert_entity_exact(
        &store,
        &EntityInput {
            name: "topic:Systems".into(),
            kind: Some("topic".into()),
            aliases: Vec::new(),
        },
    )
    .unwrap();

    let refused = |from: &str, into: &str| {
        entity_merge::merge(&store, from, into)
            .unwrap_err()
            .to_string()
    };
    assert!(refused("rusty_mux", "mux").contains("same entity"));
    assert!(refused("topic:Systems", "rusty_mux").contains("topic"));
    assert!(refused("rusty_mux", "topic:Systems").contains("topic"));
    assert!(refused("rusty_term", "rusty_mux").contains("project"));
    assert!(refused("nothing", "rusty_mux").contains("no entity"));
    // A project may absorb.
    merged(entity_merge::merge(&store, "rusty_mux", "rusty_term").unwrap());
}

#[test]
fn an_entity_with_project_claims_cannot_be_absorbed() {
    use remind_me_core::project::{set, ClaimValue, SetInput, Stage, Status};
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    entity(&store, "rusty_mux", None, &[]);
    let evidence = memory(&store, "rusty_tick was designed");
    set(
        &store,
        &SetInput {
            project: "rusty_tick".into(),
            claims: vec![
                ClaimValue::Stage(Stage::Designed),
                ClaimValue::Status(Status::Active),
            ],
            evidence: vec![evidence],
            because: None,
            confirmed_by: "human".into(),
            proposal: None,
        },
    )
    .unwrap();

    let err = entity_merge::merge(&store, "rusty_tick", "rusty_mux").unwrap_err();

    assert!(err.to_string().contains("project"), "{err}");
}

#[test]
fn a_later_mention_of_the_absorbed_name_lands_on_the_survivor() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let p = pair(&store);
    merged(entity_merge::merge(&store, "rusty-mux", "rusty_mux").unwrap());

    let again = entity(&store, "Rusty-Mux", None, &["rmux"]);

    assert_eq!(again.id, p.a.id);
    assert!(get_entity_by_id(&store, &p.b.id).unwrap().is_none());
    assert!(again.aliases.contains(&"rmux".to_string()));
}

#[test]
fn an_alias_tie_goes_to_the_oldest_and_exact_keeps_its_own_row() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let older = entity(&store, "alpha", None, &["shared"]);
    let newer = entity(&store, "beta", None, &[]);
    Entities::new(&store)
        .set_kind_and_aliases(&newer.id, None, &["shared".to_string()], &newer.updated_at)
        .unwrap();

    assert_eq!(entity(&store, "Shared", None, &[]).id, older.id);

    let own = upsert_entity_exact(
        &store,
        &EntityInput {
            name: "shared".into(),
            kind: None,
            aliases: Vec::new(),
        },
    )
    .unwrap();
    assert_eq!(own.id, entity_id("shared"));
}
