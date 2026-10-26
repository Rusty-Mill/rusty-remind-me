//! Coverage for `/api/projects` (ADR-0027).
//!
//! The route is a thin shell over `remind_me_core::project_tool::call`, which
//! has its own unit tests. What is only testable here is the shell: that a
//! `GET` reads, that a writing action over `GET` is refused rather than
//! performed, and that the core's own refusals come back as 400 rather than
//! 500.

mod common;
use common::{authed_get, call, get, seeded_server, KEY};
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::Store;
use remind_me_core::entity::{link_memory_entity, upsert_entity};
use remind_me_core::project::{ClaimValue, SetInput, Stage, Status};
use remind_me_core::EntityInput;

/// One project at `idea`, with a mention behind it.
fn seed(store: &Store<'_>) {
    // A fixed stamp: nothing here reads age, and this crate has no chrono.
    let now = "2026-10-26T12:00:00+00:00";
    let entity = upsert_entity(
        store,
        &EntityInput {
            name: "rusty_mux".into(),
            kind: None,
            aliases: vec!["mux".into()],
        },
    )
    .unwrap();
    let row = NewMemory::new("mem_seen", "we keep talking about the mux", now);
    Memories::new(store).insert(&row).unwrap();
    link_memory_entity(store, &row.id, &entity.id).unwrap();

    remind_me_core::project::set(
        store,
        &SetInput {
            project: "rusty_mux".into(),
            claims: vec![
                ClaimValue::Stage(Stage::Idea),
                ClaimValue::Status(Status::Active),
            ],
            evidence: vec![row.id.clone()],
            because: None,
            confirmed_by: "human".into(),
            proposal: None,
        },
    )
    .unwrap();
}

/// A write needs a key: unauthenticated writes are refused by the server
/// itself, which is tested in `api_keys_test.rs` rather than here.
fn post(server: &remind_me_api::ApiServer, path: &str, body: &str) -> common::Response {
    call(
        server,
        "POST",
        path,
        Some(&format!("Bearer {KEY}")),
        Some("application/json"),
        body,
    )
}

/// A server with [`KEY`] set. Every request to it needs the key, reads
/// included, so these tests use [`authed_get`] rather than [`get`].
fn authed(name: &str) -> (remind_me_api::ApiServer, std::path::PathBuf) {
    let (server, dir) = seeded_server(name, seed);
    (server.with_api_key(Some(KEY.to_string())), dir)
}

#[test]
fn get_lists_projects_and_reads_one_by_alias() {
    let (server, _dir) = seeded_server("projects_get", seed);

    let listed = get(&server, "/api/projects").json();
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["by_stage"]["idea"][0]["name"], "rusty_mux");

    let one = get(&server, "/api/projects?action=get&project=mux").json();
    assert_eq!(one["found"], true);
    assert_eq!(one["stage"]["value"], "idea");

    let unknown = get(&server, "/api/projects?action=get&project=ghost").json();
    assert_eq!(unknown["found"], false);
}

#[test]
fn a_writing_action_is_refused_over_get() {
    let (server, _dir) = seeded_server("projects_get_write", seed);

    let refused = get(&server, "/api/projects?action=set&project=rusty_mux");
    assert_eq!(refused.status, 405);
    assert!(refused.json()["error"].as_str().unwrap().contains("POST"));

    // Still at the value the fixture set: the refusal wrote nothing.
    let one = get(&server, "/api/projects?action=get&project=rusty_mux").json();
    assert_eq!(one["stage"]["value"], "idea");
}

#[test]
fn post_sets_a_claim_and_reports_what_it_superseded() {
    let (server, _dir) = authed("projects_post");

    let written = post(
        &server,
        "/api/projects",
        r#"{"action":"set","project":"rusty_mux","stage":"built","because":"the repo exists"}"#,
    );
    assert_eq!(written.status, 200);
    let payload = written.json();
    assert_eq!(payload["set"]["project"], "rusty_mux");
    assert_eq!(payload["set"]["superseded"].as_array().unwrap().len(), 1);

    let one = authed_get(&server, "/api/projects?action=get&project=rusty_mux").json();
    assert_eq!(one["stage"]["value"], "built");
}

#[test]
fn an_invalid_request_is_a_400_not_a_500() {
    let (server, _dir) = authed("projects_bad");

    assert_eq!(
        authed_get(&server, "/api/projects?action=explode").status,
        400
    );
    assert_eq!(authed_get(&server, "/api/projects?action=get").status, 400);
    assert_eq!(
        post(
            &server,
            "/api/projects",
            r#"{"action":"set","project":"rusty_mux","stage":"shipped"}"#
        )
        .status,
        400
    );
    assert_eq!(
        post(
            &server,
            "/api/projects",
            r#"{"action":"confirm","key":"nope:stage:built"}"#
        )
        .status,
        400
    );
    assert_eq!(post(&server, "/api/projects", "not json").status, 400);
}

#[test]
fn a_new_project_anchor_proposal_names_every_claim_and_the_dashboard_shows_them() {
    let (server, _dir) = seeded_server("projects_bundle", |store: &Store<'_>| {
        let entity = upsert_entity(
            store,
            &EntityInput {
                name: "rusty_tick".into(),
                kind: None,
                aliases: vec![],
            },
        )
        .unwrap();
        let mut row = NewMemory::new("mem_cwd", "worked on tick", "2026-10-26T12:00:00+00:00");
        row.cwd = Some("/src/rusty_mill/crates/apps/rusty_tick".into());
        Memories::new(store).insert(&row).unwrap();
        link_memory_entity(store, &row.id, &entity.id).unwrap();
    });

    let open = get(&server, "/api/projects?action=proposals").json();
    let path = open["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["predicate"] == "path")
        .unwrap()
        .clone();
    assert_eq!(path["with_stage"], "built");
    assert_eq!(path["with_status"], "active");

    // The pending lane renders the bundled claims, not only the anchor.
    let page = get(&server, "/").body;
    assert!(page.contains("p.with_stage") && page.contains("p.with_status"));
    assert!(page.contains("proposalClaims(p).map"));
}
