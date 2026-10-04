//! The `reference` memory_type (reference issue #220).
//!
//! The seven older types all describe a *claim*. Bulk-imported file contents —
//! source, diagrams, doc fragments — assert nothing, so they were filed as
//! `fact` for want of anywhere better, which made `fact`-filtered views a
//! mixture of real assertions and pasted-in source.
//!
//! The v28 → v29 refiling of `mempalace` facts as `reference` went with the
//! SQLite store's migrations (ADR-0025): a store copied onto the engine was
//! at v30 or later, so it had been refiled already.

use remind_me_core::db::SCHEMA_VERSION;
use remind_me_core::vitality::{get_decay_rate, get_type_prior, REFERENCE_DECAY_RATE};

// ---------------------------------------------------------------------------
// The type itself
// ---------------------------------------------------------------------------

#[test]
fn reference_has_its_own_decay_rate_and_prior_rather_than_the_fallback() {
    // The shared-database failure: without an arm of its own, `reference`
    // lands on the catch-all and ages at 0.10 while `remind_me` ages the same
    // row at 0.03.
    assert_eq!(get_decay_rate("reference"), 0.03);
    assert_eq!(get_type_prior("reference"), 0.95);

    let fallback_decay = get_decay_rate("something_nobody_defined");
    let fallback_prior = get_type_prior("something_nobody_defined");
    assert_ne!(
        get_decay_rate("reference"),
        fallback_decay,
        "reference must not be resolving through the catch-all"
    );
    assert_ne!(get_type_prior("reference"), fallback_prior);
}

#[test]
fn reference_decays_slower_than_fact_but_faster_than_decision() {
    // The ordering is the claim, not the literals: time alone does not stale a
    // snippet the way it stales a claim about current state, but the file a
    // reference mirrors changes more often than a decision is reversed.
    assert!(get_decay_rate("reference") < get_decay_rate("fact"));
    assert!(get_decay_rate("reference") > get_decay_rate("decision"));
}

#[test]
fn the_migration_constant_is_the_canonical_one() {
    // The reference has to duplicate this constant (its `vitality` imports
    // `db`, so importing back is a cycle) and guards the copy with a drift
    // test. Here there is one definition, so this asserts the wiring rather
    // than the absence of drift.
    assert_eq!(get_decay_rate("reference"), REFERENCE_DECAY_RATE);
}

#[test]
fn the_schema_version_is_past_the_refile() {
    // The number has been this crate's own since the Python reference was
    // retired (ADR-0023); 30 is vector chunks keyed by memory id; 32 the
    // context-capture columns. The engine's records mirror this version, and
    // the copy of an old `memory.db` reads 30 to 32.
    assert_eq!(SCHEMA_VERSION, 32);
    assert_eq!(remind_me_core::db::legacy_sqlite::OLDEST_COPIED_VERSION, 30);
}
