//! Direct regression coverage for the hand-written logic in `models.rs`
//! (#285).
//!
//! `models.rs` is almost entirely `Serialize`/`Deserialize` struct
//! definitions, exercised transitively through every other module's tests.
//! That is fine for plain field lists, but a handful of things in the file
//! are real, hand-written logic that a struct-shaped test elsewhere would
//! not reliably catch a regression in:
//!
//! - `Rung::as_str()` — written by hand so a variant rename can't silently
//!   orphan rows already stored under the old spelling. Already covered
//!   transitively by `promotion_test.rs::the_rung_string_form_is_stable`;
//!   the tests here duplicate that specific invariant on purpose (see that
//!   module's own doc comment) and additionally check it against the derived
//!   `Serialize` form, which `promotion_test.rs` does not.
//! - The stored category/source string constants — the same "rename silently
//!   orphans old rows" risk as `Rung::as_str()`, just for plain `&str`
//!   constants instead of an enum method.
//! - `MemorySearchInput::default()` and `ListRemindersInput::default()` —
//!   hand-written rather than derived specifically to avoid the `limit: 0`
//!   trap `MemoryListInput` has (see #223): a derived `Default` would give
//!   `limit: 0`/`token_budget: 0`, a search or list that structurally cannot
//!   return anything, with no error to say so. These tests do not touch
//!   `MemoryListInput` itself — fixing #223 is out of scope here.
//! - `ContradictionCandidatesInput::cursor()` — the one non-trivial
//!   validation method in the file, rejecting a half-supplied cursor rather
//!   than silently treating it as absent.

use remind_me_core::{
    ContradictionCandidatesInput, ListRemindersInput, MemorySearchInput, Rung, CAPTURE_SOURCE,
    DECOMPOSITION_SOURCE, DIALOG_CATEGORY, FACT_CATEGORY, PERSONA_CATEGORY, SCENARIO_CATEGORY,
    SKELETON_CATEGORY, UNCLASSIFIED,
};

#[test]
fn rung_as_str_values_are_stable() {
    // One assertion per variant, so a rename of any single variant's spelling
    // is caught immediately rather than only when some other module's test
    // happens to touch that particular rung.
    assert_eq!(Rung::CaptureToFact.as_str(), "capture_to_fact");
    assert_eq!(Rung::FactToScenario.as_str(), "fact_to_scenario");
    assert_eq!(Rung::ScenarioToPersona.as_str(), "scenario_to_persona");
}

#[test]
fn rung_serialize_matches_as_str() {
    // Two independent renderings of the same rung: the derived `Serialize`
    // (`#[serde(rename_all = "snake_case")]`, used wherever a `Rung` crosses
    // a JSON boundary) and the hand-written `as_str()` (used for the stored
    // `rung` column). They happen to agree today only because both were
    // written to match the same spelling — nothing enforces that structurally,
    // so a future edit to one without the other would silently split what a
    // client reads from what gets written to the database.
    for rung in [
        Rung::CaptureToFact,
        Rung::FactToScenario,
        Rung::ScenarioToPersona,
    ] {
        let json = serde_json::to_string(&rung).unwrap();
        assert_eq!(json, format!("\"{}\"", rung.as_str()));
    }
}

#[test]
fn category_and_source_constants_are_stable() {
    // These name the categories/sources rows are actually stored under.
    // Renaming one silently orphans every row already written under the old
    // spelling, the same failure mode `Rung::as_str()`'s doc comment warns
    // about — just spread across plain `&str` constants instead of an enum
    // method, so nothing about the type system would catch a typo here.
    assert_eq!(DIALOG_CATEGORY, "dialog");
    assert_eq!(SKELETON_CATEGORY, "skeleton");
    assert_eq!(FACT_CATEGORY, "fact");
    assert_eq!(SCENARIO_CATEGORY, "scenario");
    assert_eq!(PERSONA_CATEGORY, "persona");
    assert_eq!(CAPTURE_SOURCE, "auto_capture");
    assert_eq!(DECOMPOSITION_SOURCE, "decomposition");
    assert_eq!(UNCLASSIFIED, "unclassified");
}

#[test]
fn memory_search_input_default_matches_deserialized_minimal_json() {
    // `MemorySearchInput`'s `Default` is hand-written specifically so a
    // programmatically-built input behaves like one deserialized from a
    // minimal JSON payload that only sets `query` — see the struct's own
    // doc comment. A derived `Default` would give `limit: 0` and
    // `token_budget: 0` instead of the serde defaults of 20 / 800: not a
    // neutral starting point but a search that structurally cannot return
    // anything, with nothing to say so (the #223 trap, on a sibling field).
    let programmatic = MemorySearchInput::default();
    let from_json: MemorySearchInput = serde_json::from_str(r#"{"query": ""}"#).unwrap();

    assert_eq!(programmatic.limit, from_json.limit);
    assert_eq!(programmatic.token_budget, from_json.token_budget);
    assert_eq!(programmatic.limit, 20);
    assert_eq!(programmatic.token_budget, 800);
    assert_eq!(programmatic.response_format, from_json.response_format);
    assert_eq!(programmatic.strategy, from_json.strategy);
    assert_eq!(programmatic.include_dormant, from_json.include_dormant);
    assert_eq!(programmatic.min_vitality, from_json.min_vitality);
}

#[test]
fn list_reminders_input_default_matches_deserialized_empty_json() {
    // Same hand-written-`Default`-avoids-the-zero-trap shape as
    // `MemorySearchInput`, for `ListRemindersInput::limit`.
    let programmatic = ListRemindersInput::default();
    let from_json: ListRemindersInput = serde_json::from_str("{}").unwrap();

    assert_eq!(programmatic.when, from_json.when);
    assert_eq!(programmatic.limit, from_json.limit);
    assert_eq!(programmatic.limit, 20);
    assert_eq!(programmatic.response_format, from_json.response_format);
}

#[test]
fn contradiction_cursor_accepts_both_or_neither() {
    let neither = ContradictionCandidatesInput {
        limit: 20,
        after_a: None,
        after_b: None,
    };
    assert_eq!(neither.cursor(), Ok(None));

    let both = ContradictionCandidatesInput {
        limit: 20,
        after_a: Some("id-a".to_string()),
        after_b: Some("id-b".to_string()),
    };
    assert_eq!(both.cursor(), Ok(Some(("id-a", "id-b"))));
}

#[test]
fn contradiction_cursor_rejects_a_half_supplied_cursor() {
    // Half a cursor is rejected rather than silently treated as "no cursor" —
    // dropping it would page from the start while the caller believed it was
    // resuming, the exact bug the cursor exists to prevent, just made
    // invisible because the caller *is* passing something.
    let only_a = ContradictionCandidatesInput {
        limit: 20,
        after_a: Some("id-a".to_string()),
        after_b: None,
    };
    assert!(only_a.cursor().is_err());

    let only_b = ContradictionCandidatesInput {
        limit: 20,
        after_a: None,
        after_b: Some("id-b".to_string()),
    };
    assert!(only_b.cursor().is_err());
}
