//! Mutes (card #1485, pack D118, D124): an entity, topic or tag stops
//! appearing unasked (brief, prompt hook, digest) while an explicit search
//! still finds it, marked.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::context_brief::{self, ContextRequest, Mode, Outcome};
use remind_me_core::db::queries;
use remind_me_core::db::Store;
use remind_me_core::mute::{self, Target};
use remind_me_core::topic::{self, Parent, SetInput, Status, TopicName};
use remind_me_core::{Database, EntityInput, MemoryAddInput, MemorySearchInput};
use serde_json::json;

fn add(store: &Store<'_>, content: &str, entity: Option<&str>, tags: &[&str]) -> String {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: content.into(),
            category: "general".into(),
            source: "manual".into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            entities: entity
                .map(|name| EntityInput {
                    name: name.into(),
                    kind: Some("person".into()),
                    aliases: Vec::new(),
                })
                .into_iter()
                .collect(),
            ..Default::default()
        },
    )
    .unwrap()
    .id
}

fn seed(store: &Store<'_>) -> (String, String) {
    let muted = add(
        store,
        "Gary wants the quarterly numbers redone",
        Some("Gary"),
        &[],
    );
    let plain = add(store, "the quarterly numbers ship on friday", None, &[]);
    (muted, plain)
}

fn session_ids(store: &Store<'_>) -> Vec<String> {
    let request = ContextRequest {
        mode: Mode::Session,
        budget: 0,
        ..ContextRequest::default()
    };
    match context_brief::build(store, &request).unwrap() {
        Outcome::Brief(brief) => brief
            .sections
            .iter()
            .flat_map(|s| s.items.iter().map(|i| i.id.clone()))
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn search(store: &Store<'_>, query: &str) -> remind_me_core::expansion::MemorySearchResponse {
    let input = MemorySearchInput {
        query: query.into(),
        limit: 10,
        ..MemorySearchInput::default()
    };
    queries::search_with_expansions(store, &input).unwrap()
}

#[test]
fn a_muted_entity_leaves_the_brief_but_search_finds_it_marked() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (muted, plain) = seed(&store);
    assert!(session_ids(&store).contains(&muted));

    let record = mute::mute(&store, Target::Entity, "gary", None).unwrap();
    let ids = session_ids(&store);
    assert!(!ids.contains(&muted), "{ids:?}");
    assert!(ids.contains(&plain));
    assert!(!ids.contains(&record.id), "a mute record is never listed");

    let res = search(&store, "quarterly numbers");
    assert!(res.memories.iter().any(|r| r.memory.id == muted));
    assert!(res.muted.contains(&muted));
    assert!(!res.muted.contains(&plain));
}

#[test]
fn unmute_restores_and_muting_again_reuses_the_record() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (muted, _) = seed(&store);
    let first = mute::mute(&store, Target::Entity, "Gary", None).unwrap();

    let off = mute::unmute(&store, Target::Entity, "GARY", None)
        .unwrap()
        .unwrap();
    assert!(!off.spec.on);
    assert!(session_ids(&store).contains(&muted));
    assert!(search(&store, "quarterly numbers").muted.is_empty());

    let again = mute::mute(&store, Target::Entity, "gary", None).unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(mute::list(&store).unwrap().len(), 1);
    assert!(mute::unmute(&store, Target::Tag, "nothing", None)
        .unwrap()
        .is_none());
}

#[test]
fn a_muted_tag_is_case_blind() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let tagged = add(
        &store,
        "the offsite menu is pizza again",
        None,
        &["Offsite"],
    );
    mute::mute(&store, Target::Tag, "offsite", None).unwrap();
    assert!(!session_ids(&store).contains(&tagged));
    assert!(search(&store, "offsite menu").muted.contains(&tagged));
}

#[test]
fn a_muted_topic_hides_its_members() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let name = TopicName::parse("Payroll").unwrap();
    topic::set(
        &store,
        &SetInput {
            topic: name.clone(),
            parent: Some(Parent::Root),
            status: Some(Status::Active),
            evidence: Vec::new(),
            because: Some("seeded in a test".into()),
            confirmed_by: "human".into(),
        },
    )
    .unwrap();
    let member = add(&store, "payroll closes on the 25th", None, &[]);
    topic::assign(&store, &member, &name).unwrap();

    assert!(session_ids(&store).contains(&member));
    mute::mute(&store, Target::Topic, "payroll", None).unwrap();
    assert!(!session_ids(&store).contains(&member));
}

#[test]
fn a_project_mute_applies_only_in_that_project() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (muted, _) = seed(&store);
    queries::update_memory(
        &store,
        &remind_me_core::MemoryUpdateInput {
            memory_id: muted.clone(),
            content: None,
            category: None,
            tags: None,
            metadata: None,
            sensitive: None,
            clear_superseded: false,
            project: Some("alpha".into()),
        },
    )
    .unwrap();
    mute::mute(&store, Target::Entity, "Gary", Some("alpha")).unwrap();
    assert!(
        session_ids(&store).contains(&muted),
        "global brief unaffected"
    );
    let mut input = MemorySearchInput {
        query: "quarterly numbers".into(),
        limit: 10,
        ..MemorySearchInput::default()
    };
    input.scope.project = Some("alpha".into());
    let res = queries::search_with_expansions(&store, &input).unwrap();
    assert!(res.muted.contains(&muted));
}

#[test]
fn the_digest_leaves_muted_memories_out_and_counts_them() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (muted, plain) = seed(&store);
    mute::mute(&store, Target::Entity, "Gary", None).unwrap();
    let digest = remind_me_core::digest::build_digest(&store, 7).unwrap();
    let ids: Vec<&str> = digest
        .recent_memories
        .iter()
        .map(|m| m.id.as_str())
        .collect();
    assert!(!ids.contains(&muted.as_str()), "{ids:?}");
    assert!(ids.contains(&plain.as_str()));
    assert!(ids.iter().all(|id| !id.is_empty()));
    assert_eq!(digest.muted_hidden, 1);
    assert!(remind_me_core::digest::render_markdown(&digest).contains("1 muted memory left out"));
}

#[test]
fn a_mute_must_name_something_that_exists() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    for (target, value, says) in [
        (Target::Entity, "Nobody", "no entity"),
        (Target::Entity, "topic:Payroll", "is a topic"),
        (Target::Topic, "Nowhere", "no topic"),
    ] {
        let err = mute::mute(&store, target, value, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains(says), "{value}: {err}");
    }
    assert!(Target::parse("person").is_err());

    let err = queries::add_memory(
        &store,
        MemoryAddInput {
            content: "hand-written mute".into(),
            category: mute::CATEGORY.into(),
            source: "manual".into(),
            metadata: json!({ "mute": { "target": "tag", "value": "x", "on": true } }),
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("preference"), "{err}");
}

fn add_entity(store: &Store<'_>, name: &str, kind: &str, aliases: &[&str]) {
    queries::add_memory(
        store,
        MemoryAddInput {
            content: format!("{name} exists"),
            category: "general".into(),
            source: "manual".into(),
            entities: vec![EntityInput {
                name: name.into(),
                kind: Some(kind.into()),
                aliases: aliases.iter().map(|a| a.to_string()).collect(),
            }],
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn an_alias_and_its_entity_name_are_one_mute() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add_entity(&store, "Robert", "person", &["Bob"]);
    let first = mute::mute(&store, Target::Entity, "bob", None).unwrap();
    assert_eq!(first.spec.value, "Robert");
    let again = mute::mute(&store, Target::Entity, "Robert", None).unwrap();
    assert_eq!(again.id, first.id);
    let off = mute::unmute(&store, Target::Entity, "Robert", None)
        .unwrap()
        .unwrap();
    assert!(!off.spec.on);
    assert_eq!(mute::list(&store).unwrap().len(), 1);
}

#[test]
fn a_project_mute_matches_the_project_through_an_alias() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    add_entity(&store, "alpha", "project", &["alpha-repo"]);
    let (muted, _) = seed(&store);
    mute::mute(&store, Target::Entity, "Gary", Some("alpha-repo")).unwrap();
    let set = mute::MuteSet::load(&store, Some("ALPHA"), None).unwrap();
    let memory = remind_me_core::db::memories::Memories::new(&store)
        .get_live(&muted)
        .unwrap()
        .unwrap();
    assert!(set.covers(&memory));
}

#[test]
fn a_mute_cannot_be_reclassified_out_of_preference() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let record = mute::mute(&store, Target::Tag, "offsite", None).unwrap();
    let result = queries::reclassify_memories(
        &store,
        &remind_me_core::models::ReclassifyInput {
            classifications: vec![remind_me_core::models::MemoryClassification {
                memory_id: record.id.clone(),
                memory_type: "fact".into(),
            }],
        },
    )
    .unwrap();
    assert_eq!(result.refused, [record.id]);
    assert_eq!(mute::list(&store).unwrap().len(), 1);
}
