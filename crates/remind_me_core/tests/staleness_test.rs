//! Read-time stale flags on derived knowledge (card #60, D84, D88, D89).
//!
//! An artifact is stale when a newer, live, related memory exists, where
//! related means sharing a non-project, non-hub entity with its sources.

#[path = "../src/test_env.rs"]
mod test_env;

use chrono::{DateTime, Duration, Utc};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::Store;
use remind_me_core::entity::{link_memory_entity, upsert_entity};
use remind_me_core::promotion::{
    persona_with_staleness, promote, promotion_candidates, PromotionError,
};
use remind_me_core::staleness::{
    cited_ids, stale_check, wiki_page_staleness, CANDIDATE_CAP, HUB_LINK_CAP, NEW_EVIDENCE_MAX,
};
use remind_me_core::wiki::write_wiki_page;
use remind_me_core::{Database, EntityInput, PromoteInput, Rung, FACT_CATEGORY};

fn now() -> DateTime<Utc> {
    Utc::now()
}

fn ago(hours: i64) -> String {
    (now() - Duration::hours(hours)).to_rfc3339()
}

fn seed(store: &Store<'_>, id: &str, created_at: &str, project: Option<&str>) {
    Memories::new(store)
        .insert(&NewMemory {
            category: FACT_CATEGORY.to_string(),
            project: project.map(str::to_string),
            ..NewMemory::new(id, format!("content of {id}"), created_at)
        })
        .unwrap();
}

fn entity(store: &Store<'_>, name: &str, kind: Option<&str>) -> String {
    upsert_entity(
        store,
        &EntityInput {
            name: name.to_string(),
            kind: kind.map(str::to_string),
            aliases: Vec::new(),
        },
    )
    .unwrap()
    .id
}

fn link(store: &Store<'_>, memory_id: &str, entity_id: &str) {
    link_memory_entity(store, memory_id, entity_id).unwrap();
}

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| n.to_string()).collect()
}

/// A source fact about "Perth", written 10 hours ago, and its artifact
/// written 5 hours ago.
fn base(store: &Store<'_>) -> (String, String) {
    let perth = entity(store, "Perth", None);
    seed(store, "mem_src", &ago(10), Some("home"));
    link(store, "mem_src", &perth);
    (perth, ago(5))
}

#[test]
fn no_newer_evidence_is_fresh() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, written) = base(&store);
    seed(&store, "mem_older", &ago(8), Some("home"));
    link(&store, "mem_older", &perth);

    let s = stale_check(&store, &ids(&["mem_src"]), &written, now()).unwrap();
    assert_eq!(s.stale, Some(false));
    assert!(s.new_evidence.is_empty());
}

#[test]
fn a_newer_memory_sharing_an_entity_is_stale() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, written) = base(&store);
    seed(&store, "mem_new", &ago(1), Some("home"));
    link(&store, "mem_new", &perth);

    let s = stale_check(&store, &ids(&["mem_src"]), &written, now()).unwrap();
    assert_eq!(s.stale, Some(true));
    assert_eq!(s.new_evidence, ids(&["mem_new"]));
}

#[test]
fn sharing_only_the_project_does_not_flag() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (_, written) = base(&store);
    // Same project, an unrelated entity: D88 leaves project out.
    let other = entity(&store, "Quokka", None);
    seed(&store, "mem_same_project", &ago(1), Some("home"));
    link(&store, "mem_same_project", &other);

    let s = stale_check(&store, &ids(&["mem_src"]), &written, now()).unwrap();
    assert_eq!(s.stale, Some(false));
}

#[test]
fn a_project_entity_does_not_flag() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (_, written) = base(&store);
    let by_kind = entity(&store, "remind-me", Some("project"));
    let by_name = entity(&store, "home", None);
    for project in [&by_kind, &by_name] {
        link(&store, "mem_src", project);
    }
    seed(&store, "mem_new", &ago(1), Some("home"));
    link(&store, "mem_new", &by_kind);
    link(&store, "mem_new", &by_name);

    let s = stale_check(&store, &ids(&["mem_src"]), &written, now()).unwrap();
    assert_eq!(s.stale, Some(false));
}

#[test]
fn evidence_from_another_project_does_not_flag() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, written) = base(&store);
    seed(&store, "mem_elsewhere", &ago(1), Some("work"));
    link(&store, "mem_elsewhere", &perth);

    let s = stale_check(&store, &ids(&["mem_src"]), &written, now()).unwrap();
    assert_eq!(s.stale, Some(false));
}

#[test]
fn projectless_evidence_does_not_flag_sources_in_a_project() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, written) = base(&store);
    seed(&store, "mem_unscoped", &ago(1), None);
    link(&store, "mem_unscoped", &perth);

    let s = stale_check(&store, &ids(&["mem_src"]), &written, now()).unwrap();
    assert_eq!(s.stale, Some(false));
}

#[test]
fn an_unrelated_new_memory_does_not_flag() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (_, written) = base(&store);
    seed(&store, "mem_unlinked", &ago(1), Some("home"));

    let s = stale_check(&store, &ids(&["mem_src"]), &written, now()).unwrap();
    assert_eq!(s.stale, Some(false));
}

#[test]
fn a_new_memory_that_is_already_a_source_does_not_flag() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, written) = base(&store);
    seed(&store, "mem_new_source", &ago(1), Some("home"));
    link(&store, "mem_new_source", &perth);

    let sources = ids(&["mem_src", "mem_new_source"]);
    let s = stale_check(&store, &sources, &written, now()).unwrap();
    assert_eq!(s.stale, Some(false));
}

#[test]
fn sensitive_and_superseded_memories_do_not_flag() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, written) = base(&store);
    seed(&store, "mem_secret", &ago(1), Some("home"));
    remind_me_core::testing::set_memory_column(&store, "mem_secret", "sensitive", true).unwrap();
    link(&store, "mem_secret", &perth);
    seed(&store, "mem_replaced", &ago(1), Some("home"));
    remind_me_core::testing::set_memory_column(&store, "mem_replaced", "superseded_by", "mem_x")
        .unwrap();
    link(&store, "mem_replaced", &perth);

    let s = stale_check(&store, &ids(&["mem_src"]), &written, now()).unwrap();
    assert_eq!(s.stale, Some(false));
}

#[test]
fn a_hub_entity_is_skipped() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (_, written) = base(&store);
    let hub = entity(&store, "Everything", None);
    link(&store, "mem_src", &hub);
    for i in 0..HUB_LINK_CAP {
        let id = format!("mem_hub_{i:04}");
        seed(&store, &id, &ago(1), Some("home"));
        link(&store, &id, &hub);
    }

    let s = stale_check(&store, &ids(&["mem_src"]), &written, now()).unwrap();
    assert_eq!(
        s.stale,
        Some(false),
        "linked to {} memories",
        HUB_LINK_CAP + 1
    );
}

#[test]
fn evidence_is_capped_and_newest_first() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, written) = base(&store);
    for hours in 1..=(NEW_EVIDENCE_MAX as i64 + 2) {
        let id = format!("mem_new_{hours}");
        seed(&store, &id, &ago(hours), Some("home"));
        link(&store, &id, &perth);
    }

    let s = stale_check(&store, &ids(&["mem_src"]), &written, now()).unwrap();
    assert_eq!(s.new_evidence.len(), NEW_EVIDENCE_MAX);
    assert_eq!(s.new_evidence[0], "mem_new_1");
}

#[test]
fn the_candidate_cap_bounds_the_read() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (_, written) = base(&store);
    // Small entities whose links together pass the cap, each under the hub
    // cap, all holding older memories only. Entities are read in id order,
    // so newer evidence on the last one lies past the cap and is not seen
    // (within an entity, rows are read newest first).
    let per_entity = 200;
    let mut topics: Vec<String> = (0..CANDIDATE_CAP / per_entity + 2)
        .map(|e| entity(&store, &format!("Topic {e:02}"), None))
        .collect();
    topics.sort();
    for (e, topic) in topics.iter().enumerate() {
        link(&store, "mem_src", topic);
        for i in 0..per_entity {
            let id = format!("mem_t{e:02}_{i:03}");
            seed(&store, &id, &ago(9), Some("home"));
            link(&store, &id, topic);
        }
    }
    let last = topics.last().unwrap();
    seed(&store, "mem_past_cap", &ago(1), Some("home"));
    link(&store, "mem_past_cap", last);

    let s = stale_check(&store, &ids(&["mem_src"]), &written, now()).unwrap();
    assert_eq!(s.stale, Some(false));
}

#[test]
fn no_sources_is_unchecked() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let s = stale_check(&store, &[], &ago(1), now()).unwrap();
    assert_eq!(s.stale, None);
}

#[test]
fn a_wiki_page_is_checked_against_the_ids_it_cites() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let perth = entity(&store, "Perth", None);
    let src = "mem_0123456789abcdef0123456789abcdef";
    seed(&store, src, &ago(10), None);
    link(&store, src, &perth);
    let page = write_wiki_page(
        &store,
        "perth",
        "Perth",
        &format!("Lives in Perth (`{src}`)."),
        "",
    )
    .unwrap();
    let uncited = write_wiki_page(&store, "loose", "Loose", "No citations.", "").unwrap();

    let new = "mem_fedcba9876543210fedcba9876543210";
    let later = (now() + Duration::minutes(1)).to_rfc3339();
    seed(&store, new, &later, None);
    link(&store, new, &perth);

    let s = wiki_page_staleness(&store, &page).unwrap();
    assert_eq!(s.new_evidence, vec![new.to_string()]);
    assert_eq!(wiki_page_staleness(&store, &uncited).unwrap().stale, None);
}

#[test]
fn cited_ids_finds_each_id_once() {
    let text = "a `mem_0123abcd` b (mem_0123abcd) c xmem_99999999 d mem_ab e mem_deadbeef00";
    assert_eq!(cited_ids(text), ids(&["mem_0123abcd", "mem_deadbeef00"]));
}

/// Three facts about Perth from two origins, promoted to a scenario.
fn scenario(store: &Store<'_>) -> (String, Vec<String>, String) {
    let perth = entity(store, "Perth", None);
    let facts: Vec<String> = (0..3)
        .map(|i| {
            let id = format!("mem_fact_{i}");
            seed(store, &id, &ago(10), None);
            link(store, &id, &perth);
            id
        })
        .collect();
    let made = promote(
        store,
        &PromoteInput {
            rung: Rung::FactToScenario,
            source_ids: facts.clone(),
            content: "The user lives in Perth".into(),
            refreshes: None,
        },
    )
    .unwrap();
    (perth, facts, made.promoted_id)
}

#[test]
fn a_stale_scenario_is_a_refresh_candidate_and_refreshing_supersedes_it() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, facts, old) = scenario(&store);
    assert!(promotion_candidates(&store, Rung::FactToScenario, 20)
        .unwrap()
        .iter()
        .all(|c| c.refresh_of.is_none()));

    let later = (now() + Duration::minutes(1)).to_rfc3339();
    seed(&store, "mem_moved", &later, None);
    link(&store, "mem_moved", &perth);

    let candidates = promotion_candidates(&store, Rung::FactToScenario, 20).unwrap();
    let refresh = candidates
        .iter()
        .find(|c| c.refresh_of.as_deref() == Some(old.as_str()))
        .expect("a refresh candidate");
    assert!(refresh.reason.starts_with("refresh"));
    assert_eq!(refresh.new_evidence, ids(&["mem_moved"]));
    let mut expected = facts.clone();
    expected.push("mem_moved".to_string());
    assert_eq!(refresh.source_ids, expected);

    let new = promote(
        &store,
        &PromoteInput {
            rung: Rung::FactToScenario,
            source_ids: refresh.source_ids.clone(),
            content: "The user moved away from Perth".into(),
            refreshes: Some(old.clone()),
        },
    )
    .unwrap();
    let rows = remind_me_core::promotion::load_memories(&store, &[old]).unwrap();
    assert_eq!(
        rows[0].superseded_by.as_deref(),
        Some(new.promoted_id.as_str())
    );
}

#[test]
fn refreshing_a_persona_statement_may_keep_its_sources() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, _, scenario_id) = scenario(&store);
    let first = promote(
        &store,
        &PromoteInput {
            rung: Rung::ScenarioToPersona,
            source_ids: vec![scenario_id.clone()],
            content: "Perth-based".into(),
            refreshes: None,
        },
    )
    .unwrap();
    let later = (now() + Duration::minutes(1)).to_rfc3339();
    seed(&store, "mem_moved", &later, None);
    link(&store, "mem_moved", &perth);

    let persona = persona_with_staleness(&store).unwrap();
    assert_eq!(persona[0].stale, Some(true));
    assert_eq!(persona[0].new_evidence, ids(&["mem_moved"]));

    let again = PromoteInput {
        rung: Rung::ScenarioToPersona,
        source_ids: vec![scenario_id],
        content: "Was Perth-based".into(),
        refreshes: None,
    };
    assert!(matches!(
        promote(&store, &again),
        Err(PromotionError::DuplicateSources(_))
    ));
    let refreshed = PromoteInput {
        refreshes: Some(first.promoted_id),
        ..again
    };
    promote(&store, &refreshed).unwrap();
}

#[test]
fn refreshes_must_name_a_live_promotion_at_the_rung() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (_, facts, _) = scenario(&store);
    let bad = PromoteInput {
        rung: Rung::FactToScenario,
        source_ids: facts.clone(),
        content: "x".into(),
        refreshes: Some(facts[0].clone()),
    };
    assert!(matches!(
        promote(&store, &bad),
        Err(PromotionError::UnusableRefresh(_))
    ));
}

/// Latency on a 25k-memory store with 50k links (D84 asks for the number).
/// Run with `cargo test --release -p remind_me_core --test staleness_test
/// -- --ignored --nocapture`.
#[test]
#[ignore = "timing run, not a check"]
fn latency_on_a_25k_store() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let topics: Vec<String> = (0..500)
        .map(|e| entity(&store, &format!("Topic {e:03}"), None))
        .collect();
    for i in 0..25_000usize {
        let id = format!("mem_{i:08x}");
        seed(&store, &id, &ago((i % 2000) as i64), Some("home"));
        link(&store, &id, &topics[i % topics.len()]);
        link(&store, &id, &topics[(i * 7 + 3) % topics.len()]);
    }
    let mut times: Vec<u128> = (0..50usize)
        .map(|k| {
            let sources = vec![
                format!("mem_{:08x}", k * 97),
                format!("mem_{:08x}", k * 89 + 1),
            ];
            let started = std::time::Instant::now();
            stale_check(&store, &sources, &ago(500), now()).unwrap();
            started.elapsed().as_micros()
        })
        .collect();
    times.sort_unstable();
    println!(
        "stale_check on 25k memories / 50k links: p50 {} us, p95 {} us",
        times[times.len() / 2],
        times[times.len() * 95 / 100]
    );
}

/// `old` gets newer related evidence.
fn make_stale(store: &Store<'_>, perth: &str) {
    let later = (now() + Duration::minutes(1)).to_rfc3339();
    seed(store, "mem_moved", &later, None);
    link(store, "mem_moved", perth);
}

fn refresh_of<'a>(
    candidates: &'a [remind_me_core::PromotionCandidate],
    old: &str,
) -> Option<&'a remind_me_core::PromotionCandidate> {
    candidates
        .iter()
        .find(|c| c.refresh_of.as_deref() == Some(old))
}

#[test]
fn a_sensitive_artifact_is_not_a_refresh_candidate() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, _, old) = scenario(&store);
    make_stale(&store, &perth);
    remind_me_core::testing::set_memory_column(&store, &old, "sensitive", true).unwrap();

    let candidates = promotion_candidates(&store, Rung::FactToScenario, 20).unwrap();
    assert!(refresh_of(&candidates, &old).is_none());
}

#[test]
fn a_withdrawn_source_is_left_out_so_the_refresh_still_promotes() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, facts, old) = scenario(&store);
    make_stale(&store, &perth);
    remind_me_core::testing::set_memory_column(&store, &facts[0], "superseded_by", "mem_x")
        .unwrap();

    let candidates = promotion_candidates(&store, Rung::FactToScenario, 20).unwrap();
    let refresh = refresh_of(&candidates, &old).expect("a refresh candidate");
    assert!(!refresh.source_ids.contains(&facts[0]));
    promote(
        &store,
        &PromoteInput {
            rung: Rung::FactToScenario,
            source_ids: refresh.source_ids.clone(),
            content: "The user moved away from Perth".into(),
            refreshes: Some(old.clone()),
        },
    )
    .unwrap();
}

#[test]
fn a_statement_built_on_discredited_decisions_is_not_a_refresh_candidate() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let (perth, facts, _) = scenario(&store);
    for id in &facts {
        remind_me_core::testing::set_memory_column(&store, id, "memory_type", "decision").unwrap();
    }
    let statement = promote(
        &store,
        &PromoteInput {
            rung: Rung::ScenarioToPersona,
            source_ids: facts.clone(),
            content: "Perth-based".into(),
            refreshes: None,
        },
    )
    .unwrap()
    .promoted_id;
    for id in &facts {
        remind_me_core::testing::set_memory_column(&store, id, "outcome", "reverted").unwrap();
    }
    make_stale(&store, &perth);

    let candidates = promotion_candidates(&store, Rung::ScenarioToPersona, 20).unwrap();
    assert!(refresh_of(&candidates, &statement).is_none());
}

/// A scenario made 5 hours ago with related evidence from 3 hours ago, so
/// any write made now postdates the evidence.
fn backdated_stale_scenario(store: &Store<'_>) -> String {
    let (perth, _, old) = scenario(store);
    for column in ["created_at", "updated_at"] {
        remind_me_core::testing::set_memory_column(store, &old, column, ago(5).as_str()).unwrap();
    }
    seed(store, "mem_moved", &ago(3), None);
    link(store, "mem_moved", &perth);
    let candidates = promotion_candidates(store, Rung::FactToScenario, 20).unwrap();
    assert!(
        refresh_of(&candidates, &old).is_some(),
        "stale before the write"
    );
    old
}

fn is_refresh_candidate(store: &Store<'_>, id: &str) -> bool {
    let candidates = promotion_candidates(store, Rung::FactToScenario, 20).unwrap();
    refresh_of(&candidates, id).is_some()
}

fn update(store: &Store<'_>, fields: serde_json::Value) {
    let input: remind_me_core::MemoryUpdateInput = serde_json::from_value(fields).unwrap();
    remind_me_core::db::queries::update_memory(store, &input).unwrap();
}

#[test]
fn an_artifact_revised_after_the_evidence_is_not_stale() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = backdated_stale_scenario(&store);
    update(
        &store,
        serde_json::json!({"memory_id": old, "content": "Perth, revised"}),
    );
    assert!(!is_refresh_candidate(&store, &old));
}

#[test]
fn operational_writes_do_not_reset_staleness() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let old = backdated_stale_scenario(&store);
    let soon = (now() + Duration::days(1)).to_rfc3339();
    remind_me_core::reminders::set_reminder(&store, &old, Some(&soon)).unwrap();
    assert!(
        is_refresh_candidate(&store, &old),
        "a reminder is not a revision"
    );
    let tag = remind_me_core::BulkTagInput {
        ids: vec![old.clone()],
        tags: vec!["perth".to_string()],
        mode: Default::default(),
    };
    remind_me_core::db::queries::bulk_tag(&store, &tag).unwrap();
    assert!(
        is_refresh_candidate(&store, &old),
        "a tag edit is not a revision"
    );
    update(
        &store,
        serde_json::json!({"memory_id": old, "tags": ["perth", "wa"]}),
    );
    assert!(
        is_refresh_candidate(&store, &old),
        "nor is a tag-only update"
    );
}
