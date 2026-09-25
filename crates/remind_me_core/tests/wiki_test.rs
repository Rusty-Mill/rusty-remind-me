//! Coverage for `remind_me_wiki_list` / `remind_me_wiki_delete`.

use remind_me_core::wiki::{
    delete_wiki_page, get_wiki_page, list_wiki_pages, write_wiki_page, WikiDeleteOutcome,
};
use remind_me_core::Database;

#[test]
fn list_is_empty_on_a_fresh_wiki() {
    let db = Database::open_in_memory().unwrap();
    assert!(list_wiki_pages(&db.conn()).unwrap().is_empty());
}

#[test]
fn list_returns_pages_most_recently_updated_first() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "first", "First", "body", "general").unwrap();
    write_wiki_page(&conn, "second", "Second", "body", "general").unwrap();
    // Touch the older page so it becomes the most recent.
    write_wiki_page(&conn, "first", "First", "revised", "general").unwrap();

    let slugs: Vec<String> = list_wiki_pages(&conn)
        .unwrap()
        .into_iter()
        .map(|p| p.slug)
        .collect();
    assert_eq!(slugs.len(), 2);
    assert_eq!(slugs[0], "first", "most recently updated sorts first");
}

#[test]
fn delete_resolves_by_slug() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "vlan-setup", "VLAN Setup", "body", "network").unwrap();

    assert_eq!(
        delete_wiki_page(&conn, "vlan-setup").unwrap(),
        WikiDeleteOutcome::Deleted
    );
    assert!(get_wiki_page(&conn, "vlan-setup").unwrap().is_none());
}

#[test]
fn delete_resolves_by_title() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "vlan-setup", "VLAN Setup", "body", "network").unwrap();

    // The human title, not the slug — this is the case the reference supports
    // by running the input through slugify().
    assert_eq!(
        delete_wiki_page(&conn, "VLAN Setup").unwrap(),
        WikiDeleteOutcome::Deleted
    );
    assert!(get_wiki_page(&conn, "vlan-setup").unwrap().is_none());
}

#[test]
fn delete_tolerates_casing_and_punctuation_drift_in_the_title() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    for probe in ["VLAN  Setup!", "vlan setup", "  VLAN-Setup  "] {
        write_wiki_page(&conn, "vlan-setup", "VLAN Setup", "body", "network").unwrap();
        assert_eq!(
            delete_wiki_page(&conn, probe).unwrap(),
            WikiDeleteOutcome::Deleted,
            "{:?} should resolve to the vlan-setup slug",
            probe
        );
    }
}

#[test]
fn delete_reports_missing_pages_without_touching_others() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "keeper", "Keeper", "body", "general").unwrap();

    assert_eq!(
        delete_wiki_page(&conn, "no-such-page").unwrap(),
        WikiDeleteOutcome::NotFound
    );
    assert_eq!(list_wiki_pages(&conn).unwrap().len(), 1);
}

#[test]
fn delete_is_idempotent() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "ephemeral", "Ephemeral", "body", "general").unwrap();

    assert_eq!(
        delete_wiki_page(&conn, "ephemeral").unwrap(),
        WikiDeleteOutcome::Deleted
    );
    assert_eq!(
        delete_wiki_page(&conn, "ephemeral").unwrap(),
        WikiDeleteOutcome::NotFound,
        "a second delete is a no-op, not an error"
    );
}

#[test]
fn delete_refuses_reserved_system_pages() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    for reserved in ["index", "log", "schema"] {
        write_wiki_page(&conn, reserved, reserved, "body", "system").unwrap();
        assert_eq!(
            delete_wiki_page(&conn, reserved).unwrap(),
            WikiDeleteOutcome::Reserved,
            "{} must be protected",
            reserved
        );
        assert!(
            get_wiki_page(&conn, reserved).unwrap().is_some(),
            "{} must survive the refused delete",
            reserved
        );
    }
}

#[test]
fn delete_refuses_reserved_pages_addressed_by_title() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "index", "Index", "body", "system").unwrap();

    // "Index" slugifies to "index", so the guard must catch it here too —
    // checking the raw input instead of the slug would let this through.
    assert_eq!(
        delete_wiki_page(&conn, "Index").unwrap(),
        WikiDeleteOutcome::Reserved
    );
    assert!(get_wiki_page(&conn, "index").unwrap().is_some());
}

// --- remind_me_wiki_search (#15) -------------------------------------------

use remind_me_core::wiki::{search_wiki_pages, WIKI_SEARCH_LIMIT_MAX};

fn slugs_for(conn: &rusqlite::Connection, query: &str) -> Vec<String> {
    search_wiki_pages(conn, query, 10)
        .unwrap()
        .into_iter()
        .map(|h| h.slug)
        .collect()
}

#[test]
fn search_matches_on_title() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "vlan-setup", "VLAN Setup", "unrelated body", "").unwrap();

    assert_eq!(slugs_for(&conn, "VLAN"), vec!["vlan-setup"]);
}

#[test]
fn search_matches_on_content() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "notes", "Notes", "the quokka is a marsupial", "").unwrap();

    assert_eq!(slugs_for(&conn, "quokka"), vec!["notes"]);
}

#[test]
fn search_returns_nothing_rather_than_erroring_on_a_miss() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "notes", "Notes", "body", "").unwrap();

    assert!(slugs_for(&conn, "pangolin").is_empty());
}

#[test]
fn a_punctuation_heavy_query_is_a_search_not_a_syntax_error() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "plan", "The Plan", "what the plan is", "").unwrap();

    // Every one of ? ' , is FTS5 operator syntax. Unsanitised this is not a
    // valid MATCH expression at all, and SQLite answers with an error.
    assert_eq!(slugs_for(&conn, "what's the plan, exactly?"), vec!["plan"]);
}

#[test]
fn a_query_with_no_searchable_tokens_yields_no_hits() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "notes", "Notes", "body", "").unwrap();

    // MATCH on an empty expression is itself an error, so this must short-circuit.
    assert!(search_wiki_pages(&conn, "?! ...", 10).unwrap().is_empty());
}

#[test]
fn search_clamps_the_limit_at_both_ends() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    for i in 0..3 {
        write_wiki_page(
            &conn,
            &format!("p{}", i),
            &format!("Page {}", i),
            "shared term",
            "",
        )
        .unwrap();
    }

    assert_eq!(
        search_wiki_pages(&conn, "shared", 0).unwrap().len(),
        1,
        "a zero limit clamps up to the minimum of 1"
    );
    assert_eq!(
        search_wiki_pages(&conn, "shared", WIKI_SEARCH_LIMIT_MAX + 500)
            .unwrap()
            .len(),
        3,
        "an oversized limit clamps down without erroring"
    );
}

#[test]
fn search_carries_a_snippet_and_the_summary() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(
        &conn,
        "notes",
        "Notes",
        "a long body mentioning pangolins somewhere inside it",
        "about pangolins",
    )
    .unwrap();

    let hit = &search_wiki_pages(&conn, "pangolins", 10).unwrap()[0];
    assert_eq!(hit.summary, "about pangolins");
    assert!(
        hit.snippet.contains('[') && hit.snippet.contains(']'),
        "the matched term should be bracketed, got {:?}",
        hit.snippet
    );
}

#[test]
fn a_deleted_page_stops_matching() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "ephemeral", "Ephemeral", "quokka sighting", "").unwrap();
    assert_eq!(slugs_for(&conn, "quokka"), vec!["ephemeral"]);

    delete_wiki_page(&conn, "ephemeral").unwrap();

    // Relies on wiki_pages_ad keeping wiki_fts in step.
    assert!(slugs_for(&conn, "quokka").is_empty());
}

#[test]
fn an_edited_page_is_reindexed() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "notes", "Notes", "quokka sighting", "").unwrap();
    write_wiki_page(&conn, "notes", "Notes", "wombat sighting", "").unwrap();

    assert!(slugs_for(&conn, "quokka").is_empty(), "stale term must go");
    assert_eq!(slugs_for(&conn, "wombat"), vec!["notes"]);
}
