//! The L2/L3 context bootstrap (#255): injecting the persona into a search.
//!
//! #254 built the ladder and nothing read from it. Promoted rows are ordinary
//! memories, so they could always *match* a query like anything else — what
//! was missing is deliberate injection, independent of whether the query
//! happens to mention them. These tests are about that distinction and about
//! the two ways it could go wrong: a bootstrap that crowds out the answer, and
//! one that resurrects a statement the ladder has already withdrawn.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::queries::search_with_expansions;
use remind_me_core::models::MemorySearchInput;
use remind_me_core::promotion::{
    bootstrap, bootstrap_reserve_fraction, persona, promote, BOOTSTRAP_RESERVE_DEFAULT,
    BOOTSTRAP_RESERVE_MAX,
};
use remind_me_core::{Database, PromoteInput, Rung, FACT_CATEGORY, PERSONA_CATEGORY};
use rusqlite::{params, Connection};
use std::sync::Mutex;

/// `REMIND_ME_BOOTSTRAP_RESERVE` is process-global; serialize the tests that
/// touch it so they cannot race each other under `cargo test`'s thread pool.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn db(name: &str) -> Database {
    let dir = std::env::temp_dir().join(format!("rrm_boot_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Database::open(dir.join("memories.db").display().to_string()).unwrap()
}

fn seed(conn: &Connection, id: &str, content: &str, category: &str, sensitive: bool) {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memories (id, content, category, tags, source, metadata,
            created_at, updated_at, vitality, sensitive)
         VALUES (?, ?, ?, '[]', 'manual', '{}', ?, ?, 1.0, ?)",
        params![id, content, category, now, now, sensitive as i64],
    )
    .unwrap();
    // Planted with raw SQL, so it needs indexing to be searchable, and to be
    // deletable without the full-text index losing track of it.
    remind_me_core::db::derived::rebuild_indexes(conn).unwrap();
}

/// A persona statement with real provenance, built through `promote` so the
/// surviving-sources rule these tests lean on is the production one.
fn persona_from(conn: &Connection, source_id: &str, content: &str) -> String {
    seed(
        conn,
        source_id,
        "scenario source material",
        "scenario",
        false,
    );
    promote(
        conn,
        &PromoteInput {
            rung: Rung::ScenarioToPersona,
            source_ids: vec![source_id.to_string()],
            content: content.to_string(),
        },
    )
    .unwrap()
    .promoted_id
}

fn supersede(conn: &Connection, id: &str) {
    conn.execute(
        "UPDATE memories SET superseded_by = 'mem_newer' WHERE id = ?",
        params![id],
    )
    .unwrap();
}

fn search(
    conn: &Connection,
    query: &str,
    want_bootstrap: bool,
    budget: usize,
) -> MemorySearchInput {
    let _ = conn;
    MemorySearchInput {
        query: query.to_string(),
        token_budget: budget,
        bootstrap: want_bootstrap,
        ..Default::default()
    }
}

#[test]
fn the_bootstrap_is_off_unless_asked_for() {
    let db = db("default_off");
    let conn = db.conn();
    persona_from(&conn, "mem_src_off", "Prefers Rust over Go for services.");
    seed(
        &conn,
        "mem_hit_off",
        "quokka sightings on the island",
        FACT_CATEGORY,
        false,
    );

    assert!(
        !MemorySearchInput::default().bootstrap,
        "the struct default must be off; a derived-vs-serde split here would \
         switch the feature on for programmatic callers only"
    );

    let res = search_with_expansions(&conn, &search(&conn, "quokka", false, 800)).unwrap();
    assert!(
        res.bootstrap.is_none(),
        "not asking must produce None, not an empty bootstrap -- the two mean \
         different things to a caller"
    );
}

#[test]
fn a_persona_is_injected_even_though_it_does_not_match_the_query() {
    let db = db("injected");
    let conn = db.conn();
    persona_from(
        &conn,
        "mem_src_inj",
        "Prefers small reversible changes over big rewrites.",
    );
    seed(
        &conn,
        "mem_hit_inj",
        "quokka sightings on the island",
        FACT_CATEGORY,
        false,
    );

    let res = search_with_expansions(&conn, &search(&conn, "quokka", true, 800)).unwrap();

    let boot = res.bootstrap.expect("asked for a bootstrap");
    assert_eq!(boot.statements.len(), 1);
    assert!(boot.statements[0].content.contains("reversible"));
    assert!(boot.tokens_used > 0);

    // The point of the feature: the persona says nothing about quokkas, and
    // arrives anyway. It also stays out of the ranked list rather than being
    // reported as a match.
    assert!(
        !res.memories
            .iter()
            .any(|m| m.memory.category == PERSONA_CATEGORY),
        "the bootstrap must not be merged into the hits"
    );
    assert_eq!(res.returned, res.memories.len());
}

#[test]
fn an_empty_persona_leaves_the_hits_exactly_as_they_were() {
    let db = db("empty_persona");
    let conn = db.conn();
    seed(
        &conn,
        "mem_a",
        "quokka sightings on the island",
        FACT_CATEGORY,
        false,
    );
    seed(
        &conn,
        "mem_b",
        "quokka feeding habits",
        FACT_CATEGORY,
        false,
    );

    let without = search_with_expansions(&conn, &search(&conn, "quokka", false, 800)).unwrap();
    let with = search_with_expansions(&conn, &search(&conn, "quokka", true, 800)).unwrap();

    let ids = |r: &remind_me_core::expansion::MemorySearchResponse| {
        r.memories
            .iter()
            .map(|m| m.memory.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&without),
        ids(&with),
        "an empty persona must cost nothing"
    );
    assert_eq!(without.tokens_used, with.tokens_used);
    assert_eq!(without.budget, with.budget);

    let boot = with.bootstrap.expect("asked for one");
    assert!(boot.is_empty());
    assert_eq!(boot.tokens_used, 0);
}

#[test]
fn a_large_persona_cannot_starve_the_ranked_results() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The pathological setting: a caller asking for the whole budget to go to
    // the persona. The cap is what makes this survivable, so test it at the
    // value that would break it rather than at the default.
    crate::test_env::set_var("REMIND_ME_BOOTSTRAP_RESERVE", "4.0");

    let db = db("starvation");
    let conn = db.conn();
    for i in 0..40 {
        persona_from(
            &conn,
            &format!("mem_src_big_{}", i),
            &format!(
                "Durable statement number {} about how this person works, written \
                 long enough to cost a meaningful number of tokens on its own.",
                i
            ),
        );
    }
    seed(
        &conn,
        "mem_hit_big",
        "quokka sightings on the island",
        FACT_CATEGORY,
        false,
    );

    let budget = 400usize;
    let res = search_with_expansions(&conn, &search(&conn, "quokka", true, budget)).unwrap();
    crate::test_env::remove_var("REMIND_ME_BOOTSTRAP_RESERVE");

    let boot = res.bootstrap.expect("asked for one");
    let ceiling = (budget as f64 * BOOTSTRAP_RESERVE_MAX) as usize;
    assert!(
        boot.tokens_used <= ceiling,
        "bootstrap spent {} of a {} budget; the {:.0}% cap should have held it to {}",
        boot.tokens_used,
        budget,
        BOOTSTRAP_RESERVE_MAX * 100.0,
        ceiling
    );
    assert!(boot.omitted > 0, "40 statements cannot all have fit");
    assert!(
        !res.memories.is_empty(),
        "the query still has to be answered -- this is the floor the cap exists to provide"
    );
    // The caller asked for 400 and must be told 400, not what was left after
    // the reserve was taken out of it.
    assert_eq!(res.budget, budget);
}

#[test]
fn superseding_the_last_source_withdraws_the_statement_from_the_bootstrap_too() {
    let db = db("demotion");
    let conn = db.conn();
    persona_from(&conn, "mem_src_dem", "Ships on Fridays without ceremony.");
    seed(
        &conn,
        "mem_hit_dem",
        "quokka sightings on the island",
        FACT_CATEGORY,
        false,
    );

    assert_eq!(persona(&conn).unwrap().len(), 1);
    assert_eq!(bootstrap(&conn, 800).unwrap().statements.len(), 1);

    supersede(&conn, "mem_src_dem");

    // One code path, so these cannot disagree -- which is the guarantee, not
    // an incidental consequence. A separate query in the bootstrap would let
    // a withdrawn statement keep being injected into every search while
    // `remind_me_persona` correctly reported it gone.
    assert!(persona(&conn).unwrap().is_empty());
    assert!(bootstrap(&conn, 800).unwrap().is_empty());

    let res = search_with_expansions(&conn, &search(&conn, "quokka", true, 800)).unwrap();
    assert!(res.bootstrap.expect("asked for one").is_empty());
}

#[test]
fn sensitive_statements_never_enter_the_bootstrap() {
    let db = db("sensitive");
    let conn = db.conn();

    // Seeded rather than promoted: `promote` refuses a sensitive source for
    // the persona rung, so this is the only way such a row can exist -- an
    // older row, or one marked sensitive after the fact. That is precisely the
    // case the read-side filter has to cover.
    seed(
        &conn,
        "mem_sens_src",
        "scenario source material",
        "scenario",
        false,
    );
    seed(
        &conn,
        "mem_sens_persona",
        "Sensitive durable statement that must never be injected.",
        PERSONA_CATEGORY,
        true,
    );
    conn.execute(
        "INSERT INTO promotions (promoted_id, source_id, rung, promoted_at)
         VALUES ('mem_sens_persona', 'mem_sens_src', 'scenario_to_persona', ?)",
        params![chrono::Utc::now().to_rfc3339()],
    )
    .unwrap();

    // Its provenance is intact, so it is withheld for being sensitive rather
    // than for having lost its grounds -- otherwise this would pass for the
    // wrong reason.
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM promotions WHERE promoted_id = 'mem_sens_persona'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert!(bootstrap(&conn, 800).unwrap().is_empty());

    // Even asking for sensitive content in the search does not reach it: the
    // bootstrap is ambient rather than requested per-item, so there is no
    // per-call intent to opt back in against.
    let mut input = search(&conn, "quokka", true, 800);
    input.include_sensitive = true;
    let res = search_with_expansions(&conn, &input).unwrap();
    assert!(res.bootstrap.expect("asked for one").is_empty());
}

#[test]
fn the_reserve_fraction_is_clamped_rather_than_trusted() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Asserted directly because the starvation test above would pass whether
    // the clamp engaged or the default did -- 0.25 and a clamped 0.5 both sit
    // under the ceiling it checks. This is the assertion that fails if the
    // clamp is removed.
    crate::test_env::set_var("REMIND_ME_BOOTSTRAP_RESERVE", "4.0");
    assert_eq!(bootstrap_reserve_fraction(), BOOTSTRAP_RESERVE_MAX);

    // A percentage typed where a fraction was wanted is the likely slip, and
    // it must not hand the whole budget to the persona either.
    crate::test_env::set_var("REMIND_ME_BOOTSTRAP_RESERVE", "90");
    assert_eq!(bootstrap_reserve_fraction(), BOOTSTRAP_RESERVE_MAX);

    // Malformed input falls back rather than failing the search around it.
    crate::test_env::set_var("REMIND_ME_BOOTSTRAP_RESERVE", "not-a-number");
    assert_eq!(bootstrap_reserve_fraction(), BOOTSTRAP_RESERVE_DEFAULT);

    crate::test_env::set_var("REMIND_ME_BOOTSTRAP_RESERVE", "-1");
    assert_eq!(bootstrap_reserve_fraction(), BOOTSTRAP_RESERVE_DEFAULT);

    // Zero is a legitimate setting -- "assemble no bootstrap" -- and must not
    // be mistaken for unset.
    crate::test_env::set_var("REMIND_ME_BOOTSTRAP_RESERVE", "0");
    assert_eq!(bootstrap_reserve_fraction(), 0.0);

    crate::test_env::remove_var("REMIND_ME_BOOTSTRAP_RESERVE");
    assert_eq!(bootstrap_reserve_fraction(), BOOTSTRAP_RESERVE_DEFAULT);
}

#[test]
fn an_unlimited_budget_takes_the_whole_persona() {
    let db = db("unlimited");
    let conn = db.conn();
    for i in 0..5 {
        persona_from(
            &conn,
            &format!("mem_src_unl_{}", i),
            &format!("Durable statement number {}.", i),
        );
    }

    let boot = bootstrap(&conn, 0).unwrap();
    assert_eq!(
        boot.statements.len(),
        5,
        "0 means unlimited, as it does for the hits"
    );
    assert_eq!(boot.omitted, 0);
    assert!(
        boot.tokens_used > 0,
        "still counted, so 'how big was this' stays answerable"
    );
}
