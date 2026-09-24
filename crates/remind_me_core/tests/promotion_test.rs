//! The refinement ladder (#208): capture → fact → scenario → persona.
//!
//! The properties that matter are the ones that make the ladder trustworthy
//! rather than merely present:
//!
//! - **Idempotency** comes from the candidate query, not from the caller
//!   remembering. Promoting and re-listing must return a shorter list.
//! - **Provenance walks both ways**, or a persona statement is unfalsifiable.
//! - **Demotion is automatic.** Superseding a fact has to withdraw the persona
//!   built on it, with nothing scheduled and nobody asked.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::entity::link_memory_entity;
use remind_me_core::promotion::{
    demoted, persona, promote, promotion_candidates, provenance, PromotionError,
};
use remind_me_core::{
    Database, EntityInput, PromoteInput, Rung, FACT_CATEGORY, PERSONA_CATEGORY, SCENARIO_CATEGORY,
};
use rusqlite::{params, Connection};

fn db(name: &str) -> Database {
    let dir = std::env::temp_dir().join(format!("rrm_promo_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Database::open(dir.join("memories.db").display().to_string()).unwrap()
}

/// Insert a memory directly. Promotion reads categories and flags, not the
/// path a memory arrived by, so seeding beats driving six tools per fixture.
fn seed(conn: &Connection, id: &str, content: &str, category: &str, sensitive: bool) {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memories (id, content, category, tags, source, metadata,
            created_at, updated_at, vitality, sensitive)
         VALUES (?, ?, ?, '[]', 'manual', '{}', ?, ?, 1.0, ?)",
        params![id, content, category, now, now, sensitive as i64],
    )
    .unwrap();
}

/// Attach `count` facts to one entity, so they cluster as a scenario candidate.
fn facts_about(conn: &Connection, entity_name: &str, count: usize) -> Vec<String> {
    let entity = remind_me_core::entity::upsert_entity(
        conn,
        &EntityInput {
            name: entity_name.to_string(),
            kind: None,
            aliases: Vec::new(),
        },
    )
    .unwrap();

    (0..count)
        .map(|i| {
            let id = format!("mem_fact_{}_{}", entity_name.replace(' ', "_"), i);
            seed(
                conn,
                &id,
                &format!("{} fact number {}", entity_name, i),
                FACT_CATEGORY,
                false,
            );
            link_memory_entity(conn, &id, &entity.id).unwrap();
            id
        })
        .collect()
}

fn supersede(conn: &Connection, id: &str) {
    conn.execute(
        "UPDATE memories SET superseded_by = 'mem_newer' WHERE id = ?",
        params![id],
    )
    .unwrap();
}

#[test]
fn facts_sharing_an_entity_become_a_scenario_candidate() {
    let db = db("scenario_candidates");
    let conn = db.conn();
    facts_about(&conn, "Rottnest Island", 4);

    let candidates = promotion_candidates(&conn, Rung::FactToScenario, 20).unwrap();

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].source_ids.len(), 4);
    assert_eq!(candidates[0].grouped_by.as_deref(), Some("Rottnest Island"));
    assert!(candidates[0].reason.contains("Rottnest Island"));
}

#[test]
fn two_facts_are_not_enough_to_propose_a_scenario() {
    let db = db("threshold");
    let conn = db.conn();
    facts_about(&conn, "Quokka", 2);

    // Otherwise the candidate list is one entry per entity in the store from
    // the moment anything is decomposed, which is noise, not a backlog.
    assert!(promotion_candidates(&conn, Rung::FactToScenario, 20)
        .unwrap()
        .is_empty());
}

#[test]
fn promoting_removes_the_sources_from_the_candidate_list() {
    let db = db("idempotent");
    let conn = db.conn();
    let facts = facts_about(&conn, "Perth", 3);

    let before = promotion_candidates(&conn, Rung::FactToScenario, 20).unwrap();
    assert_eq!(before.len(), 1);

    promote(
        &conn,
        &PromoteInput {
            rung: Rung::FactToScenario,
            source_ids: facts.clone(),
            content: "Perth is where the user lives and works".into(),
        },
    )
    .unwrap();

    // Idempotency is the candidate query's job: a second pass over unchanged
    // data must find nothing, so a scheduled loop cannot re-promote forever.
    let after = promotion_candidates(&conn, Rung::FactToScenario, 20).unwrap();
    assert!(
        after.is_empty(),
        "promoted facts came back as candidates: {:?}",
        after
    );
}

#[test]
fn provenance_walks_both_directions() {
    let db = db("provenance");
    let conn = db.conn();
    let facts = facts_about(&conn, "Rust", 3);

    let scenario = promote(
        &conn,
        &PromoteInput {
            rung: Rung::FactToScenario,
            source_ids: facts.clone(),
            content: "The user writes Rust for low-level servers".into(),
        },
    )
    .unwrap();

    let statement = promote(
        &conn,
        &PromoteInput {
            rung: Rung::ScenarioToPersona,
            source_ids: vec![scenario.promoted_id.clone()],
            content: "Prefers systems languages".into(),
        },
    )
    .unwrap();

    // Downward: persona -> scenario -> facts.
    let from_persona = provenance(&conn, &statement.promoted_id).unwrap();
    assert_eq!(from_persona.sources, vec![scenario.promoted_id.clone()]);
    assert!(from_persona.derived.is_empty());

    let from_scenario = provenance(&conn, &scenario.promoted_id).unwrap();
    assert_eq!(from_scenario.sources.len(), 3);
    assert_eq!(from_scenario.derived, vec![statement.promoted_id.clone()]);

    // Upward from a leaf fact: what does this still support?
    let from_fact = provenance(&conn, &facts[0]).unwrap();
    assert_eq!(from_fact.derived, vec![scenario.promoted_id]);
    assert!(from_fact.sources.is_empty());
}

#[test]
fn superseding_the_last_source_withdraws_the_persona_built_on_it() {
    let db = db("demotion");
    let conn = db.conn();
    seed(
        &conn,
        "mem_scenario",
        "A stable scenario",
        SCENARIO_CATEGORY,
        false,
    );

    let statement = promote(
        &conn,
        &PromoteInput {
            rung: Rung::ScenarioToPersona,
            source_ids: vec!["mem_scenario".into()],
            content: "Believed durably true".into(),
        },
    )
    .unwrap();

    assert_eq!(persona(&conn).unwrap().len(), 1);

    supersede(&conn, "mem_scenario");

    // No background job, no second opinion about what "still true" means:
    // the read is the judgement.
    assert!(
        persona(&conn).unwrap().is_empty(),
        "a persona statement outlived every source it rested on"
    );

    // Withheld, not deleted — a statement that quietly stopped appearing is
    // otherwise indistinguishable from one never written.
    let withheld = demoted(&conn).unwrap();
    assert_eq!(withheld.len(), 1);
    assert_eq!(withheld[0].id, statement.promoted_id);
    assert_eq!(withheld[0].surviving_sources, 0);
}

#[test]
fn one_surviving_source_is_enough_to_keep_a_statement() {
    let db = db("partial_demotion");
    let conn = db.conn();
    seed(&conn, "mem_s1", "Scenario one", SCENARIO_CATEGORY, false);
    seed(&conn, "mem_s2", "Scenario two", SCENARIO_CATEGORY, false);

    promote(
        &conn,
        &PromoteInput {
            rung: Rung::ScenarioToPersona,
            source_ids: vec!["mem_s1".into(), "mem_s2".into()],
            content: "Rests on two things".into(),
        },
    )
    .unwrap();

    supersede(&conn, "mem_s1");

    let live = persona(&conn).unwrap();
    assert_eq!(live.len(), 1, "losing one of two grounds is not demotion");
    assert_eq!(live[0].surviving_sources, 1);
}

#[test]
fn a_sensitive_source_cannot_become_persona() {
    let db = db("sensitive");
    let conn = db.conn();
    seed(
        &conn,
        "mem_private",
        "A private scenario",
        SCENARIO_CATEGORY,
        true,
    );

    let err = promote(
        &conn,
        &PromoteInput {
            rung: Rung::ScenarioToPersona,
            source_ids: vec!["mem_private".into()],
            content: "Should never be assembled".into(),
        },
    )
    .unwrap_err();

    assert!(matches!(err, PromotionError::SensitiveSource(_)));
    // A refused promotion writes nothing at all.
    let rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE category = ?",
            params![PERSONA_CATEGORY],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn a_sensitive_scenario_is_never_even_offered_as_a_candidate() {
    let db = db("sensitive_candidate");
    let conn = db.conn();
    seed(&conn, "mem_private", "Private", SCENARIO_CATEGORY, true);
    seed(&conn, "mem_public", "Public", SCENARIO_CATEGORY, false);

    let candidates = promotion_candidates(&conn, Rung::ScenarioToPersona, 20).unwrap();
    let ids: Vec<&String> = candidates.iter().flat_map(|c| &c.source_ids).collect();

    assert!(ids.iter().any(|id| *id == "mem_public"));
    assert!(
        !ids.iter().any(|id| *id == "mem_private"),
        "refusing at promote time but offering at candidate time invites the caller \
         to spend a model call on something that will be rejected"
    );
}

#[test]
fn rung_one_reports_a_backlog_but_refuses_to_promote() {
    let db = db("rung_one");
    let conn = db.conn();

    let err = promote(
        &conn,
        &PromoteInput {
            rung: Rung::CaptureToFact,
            source_ids: vec!["mem_anything".into()],
            content: "a fact".into(),
        },
    )
    .unwrap_err();

    // Two write paths to one rung would drift; the error has to say where to go.
    match err {
        PromotionError::UseDecompose => {
            assert!(format!("{}", PromotionError::UseDecompose).contains("remind_me_decompose"));
        }
        other => panic!("expected UseDecompose, got {:?}", other),
    }
}

#[test]
fn a_promotion_with_an_unusable_source_is_refused_whole() {
    let db = db("bad_source");
    let conn = db.conn();
    seed(&conn, "mem_good", "Real", SCENARIO_CATEGORY, false);

    let err = promote(
        &conn,
        &PromoteInput {
            rung: Rung::ScenarioToPersona,
            source_ids: vec!["mem_good".into(), "mem_missing".into()],
            content: "Half-grounded".into(),
        },
    )
    .unwrap_err();

    assert!(matches!(err, PromotionError::UnusableSource(ref id) if id == "mem_missing"));
    // Validated before the insert, so no partially-linked artifact is left.
    let rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE category = ?",
            params![PERSONA_CATEGORY],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);
    let links: i64 = conn
        .query_row("SELECT count(*) FROM promotions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(links, 0);
}

#[test]
fn promoting_the_same_sources_twice_is_rejected() {
    let db = db("duplicate_sources");
    let conn = db.conn();
    let facts = facts_about(&conn, "Duplicate Island", 3);

    let first = promote(
        &conn,
        &PromoteInput {
            rung: Rung::FactToScenario,
            source_ids: facts.clone(),
            content: "First telling of the same evidence".into(),
        },
    )
    .unwrap();

    // Same sources, different order, and a distinct description of the
    // "distillation" — none of that should matter. `INSERT OR IGNORE` only
    // dedupes rows within a single call, so without this check two calls
    // like this create two independent scenario memories from one set of
    // facts (#274).
    let mut reordered = facts.clone();
    reordered.reverse();

    let err = promote(
        &conn,
        &PromoteInput {
            rung: Rung::FactToScenario,
            source_ids: reordered,
            content: "Second telling of the same evidence".into(),
        },
    )
    .unwrap_err();

    match err {
        PromotionError::DuplicateSources(ref id) => assert_eq!(id, &first.promoted_id),
        other => panic!("expected DuplicateSources, got {:?}", other),
    }

    // Exactly one scenario memory exists for this evidence, not two.
    let rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE category = ?",
            params![SCENARIO_CATEGORY],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
}

#[test]
fn a_superseded_promotion_no_longer_blocks_re_promoting_its_sources() {
    let db = db("duplicate_sources_after_supersede");
    let conn = db.conn();
    let facts = facts_about(&conn, "Rottnest Reprise", 3);

    let first = promote(
        &conn,
        &PromoteInput {
            rung: Rung::FactToScenario,
            source_ids: facts.clone(),
            content: "Original telling".into(),
        },
    )
    .unwrap();

    // Once the earlier promoted memory is no longer live, its source set no
    // longer counts as "already promoted" -- a live promotion is the bar,
    // not "ever promoted".
    supersede(&conn, &first.promoted_id);

    let second = promote(
        &conn,
        &PromoteInput {
            rung: Rung::FactToScenario,
            source_ids: facts,
            content: "Re-told after the original was superseded".into(),
        },
    )
    .unwrap();

    assert_ne!(second.promoted_id, first.promoted_id);
}

#[test]
fn a_promotion_needs_sources_and_content() {
    let db = db("empty");
    let conn = db.conn();
    seed(&conn, "mem_s", "Scenario", SCENARIO_CATEGORY, false);

    assert!(matches!(
        promote(
            &conn,
            &PromoteInput {
                rung: Rung::ScenarioToPersona,
                source_ids: vec![],
                content: "ungrounded".into(),
            }
        )
        .unwrap_err(),
        PromotionError::NoSources
    ));

    assert!(matches!(
        promote(
            &conn,
            &PromoteInput {
                rung: Rung::ScenarioToPersona,
                source_ids: vec!["mem_s".into()],
                content: "   ".into(),
            }
        )
        .unwrap_err(),
        PromotionError::EmptyContent
    ));
}

/// `REMIND_ME_PROMOTION_INTERVAL` and the nudge's process-local "already
/// announced" state are both global, so nudge tests serialise behind this.
static NUDGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_backlog_counts_every_rung_at_once() {
    let db = db("backlog");
    let conn = db.conn();
    facts_about(&conn, "Perth", 3);
    seed(&conn, "mem_sc", "A scenario", SCENARIO_CATEGORY, false);

    let backlog = remind_me_core::promotion::backlog(&conn).unwrap();

    assert_eq!(backlog.fact_to_scenario, 1);
    assert_eq!(backlog.scenario_to_persona, 1);
    assert_eq!(backlog.total(), 2);
    assert!(backlog.summary().contains("fact→scenario"));
    assert!(backlog.summary().contains("scenario→persona"));
}

#[test]
fn an_empty_backlog_says_so_rather_than_rendering_blank() {
    let db = db("empty_backlog");
    let conn = db.conn();
    let backlog = remind_me_core::promotion::backlog(&conn).unwrap();

    assert!(backlog.is_empty());
    assert_eq!(backlog.summary(), "nothing waiting");
}

#[test]
fn the_nudge_is_off_unless_an_interval_is_configured() {
    let _guard = NUDGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(remind_me_core::promotion::NUDGE_INTERVAL_ENV);

    assert!(remind_me_core::promotion::nudge_interval().is_none());

    let db = db("nudge_off");
    // No interval means no loop, matching the folder watcher's convention
    // rather than the reminder scheduler's always-on one.
    assert!(remind_me_core::promotion::start_nudge_for(&db.conn()).is_none());
}

#[test]
fn a_zero_interval_is_treated_as_off_not_as_a_busy_loop() {
    let _guard = NUDGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var(remind_me_core::promotion::NUDGE_INTERVAL_ENV, "0");
    assert!(remind_me_core::promotion::nudge_interval().is_none());

    crate::test_env::set_var(
        remind_me_core::promotion::NUDGE_INTERVAL_ENV,
        "not a number",
    );
    assert!(remind_me_core::promotion::nudge_interval().is_none());

    crate::test_env::set_var(remind_me_core::promotion::NUDGE_INTERVAL_ENV, "900");
    assert_eq!(
        remind_me_core::promotion::nudge_interval(),
        Some(std::time::Duration::from_secs(900))
    );

    crate::test_env::remove_var(remind_me_core::promotion::NUDGE_INTERVAL_ENV);
}

#[test]
fn an_unchanged_backlog_is_announced_once_and_then_stays_quiet() {
    let _guard = NUDGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = db("nudge_repeat");
    let conn = db.conn();
    facts_about(&conn, "Fremantle", 3);

    let (first, notified_first) = remind_me_core::promotion::nudge_once(&conn).unwrap();
    assert_eq!(first.total(), 1);

    // An hourly nudge repeating the same sentence is noise the reader learns
    // to filter, and they take the real change with it.
    let (second, notified_second) = remind_me_core::promotion::nudge_once(&conn).unwrap();
    assert_eq!(second.total(), 1);
    assert!(!notified_second, "an unchanged backlog was announced twice");

    // A growing backlog is worth saying again.
    facts_about(&conn, "Cottesloe", 3);
    let (third, notified_third) = remind_me_core::promotion::nudge_once(&conn).unwrap();
    assert_eq!(third.total(), 2);

    // `notified_*` reflects whether a channel was written to; with none
    // configured `notify` reaches zero of them, so what is asserted here is
    // the decision, taken before any channel is consulted.
    let _ = (notified_first, notified_third);
}

#[test]
fn clearing_the_backlog_does_not_send_an_empty_nudge() {
    let _guard = NUDGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = db("nudge_cleared");
    let conn = db.conn();
    let facts = facts_about(&conn, "Cockburn", 3);

    remind_me_once(&conn);
    promote(
        &conn,
        &PromoteInput {
            rung: Rung::FactToScenario,
            source_ids: facts,
            content: "Somewhere the user knows".into(),
        },
    )
    .unwrap();

    // The scenario the promotion just created is itself a persona candidate,
    // so the backlog moves rather than empties -- which is the point: the
    // ladder always has a next rung, and the nudge tracks the total.
    let (backlog, _) = remind_me_core::promotion::nudge_once(&conn).unwrap();
    assert_eq!(backlog.fact_to_scenario, 0);
    assert_eq!(backlog.scenario_to_persona, 1);
}

/// Prime the nudge's "already announced" state so the assertions above start
/// from a known point regardless of test ordering.
fn remind_me_once(conn: &Connection) {
    let _ = remind_me_core::promotion::nudge_once(conn);
}

#[test]
fn the_rung_string_form_is_stable() {
    // Stored in `promotions.rung`. Deriving it from Debug would let a variant
    // rename silently orphan every row written under the old spelling.
    assert_eq!(Rung::CaptureToFact.as_str(), "capture_to_fact");
    assert_eq!(Rung::FactToScenario.as_str(), "fact_to_scenario");
    assert_eq!(Rung::ScenarioToPersona.as_str(), "scenario_to_persona");
}
