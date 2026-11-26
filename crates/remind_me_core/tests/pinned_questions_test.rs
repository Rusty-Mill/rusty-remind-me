//! Pinned questions: standing answers that know when they are out of date
//! (card #61, D85, D90).

#[path = "../src/test_env.rs"]
mod test_env;

use chrono::{Duration, Utc};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::Store;
use remind_me_core::saved_searches::{
    answer_pinned, get_saved_search, pin_saved_search, save_search, stale_pinned, StaleReason,
};
use remind_me_core::wiki::write_wiki_page;
use remind_me_core::{AnswerPinnedInput, Database, SaveSearchInput};

fn seed(store: &Store<'_>, id: &str, content: &str, created_at: &str) {
    Memories::new(store)
        .insert(&NewMemory::new(id, content, created_at))
        .unwrap();
}

fn ago(minutes: i64) -> String {
    (Utc::now() - Duration::minutes(minutes)).to_rfc3339()
}

fn save(store: &Store<'_>, name: &str, query: &str) {
    save_search(
        store,
        &SaveSearchInput {
            name: name.to_string(),
            query: query.to_string(),
            category: None,
            tags: None,
            include_sensitive: false,
            watch: false,
        },
    )
    .unwrap();
}

fn answer(store: &Store<'_>, ids: &[&str]) -> remind_me_core::db::Result<()> {
    answer_pinned(
        store,
        &AnswerPinnedInput {
            name: "deploys".to_string(),
            answer_slug: "deploys".to_string(),
            evidence_ids: ids.iter().map(|i| i.to_string()).collect(),
        },
    )
    .map(drop)
}

/// A pinned "deploys" question answered from two memories.
fn answered(store: &Store<'_>) {
    seed(store, "mem_a", "deploy runs on fridays", &ago(30));
    seed(store, "mem_b", "deploy needs a backup first", &ago(20));
    save(store, "deploys", "deploy");
    write_wiki_page(store, "deploys", "Deploys", "Fridays, after a backup.", "").unwrap();
    pin_saved_search(store, "deploys").unwrap().unwrap();
    answer(store, &["mem_a", "mem_b"]).unwrap();
}

fn reasons(store: &Store<'_>) -> Vec<StaleReason> {
    stale_pinned(store)
        .unwrap()
        .into_iter()
        .flat_map(|s| s.reasons)
        .collect()
}

#[test]
fn a_pinned_unanswered_question_is_stale() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    save(&store, "deploys", "deploy");
    assert!(stale_pinned(&store).unwrap().is_empty(), "not pinned yet");

    pin_saved_search(&store, "deploys").unwrap().unwrap();
    assert_eq!(reasons(&store), vec![StaleReason::Unanswered]);
    assert!(pin_saved_search(&store, "missing").unwrap().is_none());
}

#[test]
fn a_fresh_answer_is_not_stale() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    answered(&store);
    assert!(stale_pinned(&store).unwrap().is_empty());
}

#[test]
fn a_new_result_makes_it_stale_and_answering_again_clears_it() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    answered(&store);
    let later = (Utc::now() + Duration::minutes(1)).to_rfc3339();
    seed(&store, "mem_c", "deploy moved to mondays", &later);

    let stale = stale_pinned(&store).unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].reasons, vec![StaleReason::NewResults]);
    assert_eq!(stale[0].new_evidence, vec!["mem_c".to_string()]);

    answer(&store, &["mem_a", "mem_b", "mem_c"]).unwrap();
    assert!(stale_pinned(&store).unwrap().is_empty());
}

#[test]
fn an_older_memory_entering_the_results_does_not_flag() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    answered(&store);
    // Written before the answer, it matched all along; ranking alone
    // moving it into view is not new evidence.
    seed(&store, "mem_old", "deploy notes from last year", &ago(600));
    assert!(stale_pinned(&store).unwrap().is_empty());
}

#[test]
fn deleted_evidence_makes_it_stale() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    answered(&store);
    remind_me_core::testing::set_memory_column(&store, "mem_b", "superseded_by", "mem_x").unwrap();
    let stale = stale_pinned(&store).unwrap();
    assert_eq!(stale[0].reasons, vec![StaleReason::EvidenceGone]);
    assert_eq!(stale[0].evidence_gone, vec!["mem_b".to_string()]);
}

#[test]
fn a_missing_answer_page_makes_it_stale() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    answered(&store);
    remind_me_core::wiki::delete_wiki_page(&store, "deploys").unwrap();
    assert_eq!(reasons(&store), vec![StaleReason::AnswerMissing]);
}

#[test]
fn resaving_keeps_the_pin_and_a_changed_query_is_stale() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    answered(&store);
    save(&store, "deploys", "deploy");
    let kept = get_saved_search(&store, "deploys").unwrap().unwrap();
    assert!(kept.filters.pin.unwrap().answer_slug.is_some());
    assert!(stale_pinned(&store).unwrap().is_empty());

    save(&store, "deploys", "deploy friday");
    assert!(reasons(&store).contains(&StaleReason::QueryChanged));
}

#[test]
fn answer_validates_its_input() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    answered(&store);
    assert!(answer(&store, &[]).is_err(), "no ids");
    assert!(answer(&store, &["mem_nope"]).is_err(), "unknown id");
    let too_many: Vec<String> = (0..=remind_me_core::PIN_EVIDENCE_MAX)
        .map(|i| format!("mem_{i}"))
        .collect();
    let refs: Vec<&str> = too_many.iter().map(String::as_str).collect();
    assert!(answer(&store, &refs).is_err(), "too many ids");
    let no_page = AnswerPinnedInput {
        name: "deploys".to_string(),
        answer_slug: "nowhere".to_string(),
        evidence_ids: vec!["mem_a".to_string()],
    };
    assert!(answer_pinned(&store, &no_page).is_err(), "missing page");
}
