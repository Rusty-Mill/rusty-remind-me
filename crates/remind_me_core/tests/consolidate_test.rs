//! Coverage for `remind_me_consolidate`'s DB-touching orchestration layer.
//!
//! The pure clustering/merge algorithm ([`find_clusters`], [`pick_canonical`],
//! [`merge_cluster`]) has its own unit tests inline in
//! `remind_me_core::consolidation`, alongside the code they test — this file
//! only covers what actually needs a live store: candidate fetching, category
//! scoping, the `limit` cap, dry-run's no-op guarantee, and the write path a
//! real merge takes.
//!
//! Embeddings are written directly via raw SQL (bypassing the embedder),
//! matching `vectors_test.rs`'s convention — chunk 0 is what
//! `consolidation::consolidate` reads.

use remind_me_core::consolidation::consolidate;
use remind_me_core::db::queries;
use remind_me_core::{ConsolidateInput, Database, MemoryAddInput};
use rusqlite::Connection;
use std::collections::HashMap;

fn add_with_vector(conn: &Connection, content: &str, category: &str, vector: &[f32]) -> String {
    let id = queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: category.to_string(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap()
    .id;

    let rowid: i64 = conn
        .query_row("SELECT rowid FROM memories WHERE id = ?", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    conn.execute(
        "INSERT INTO vec_chunks (memory_rowid, chunk_ix) VALUES (?, 0)",
        [rowid],
    )
    .unwrap();
    let vec_rowid = conn.last_insert_rowid();
    let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
    conn.execute(
        "INSERT INTO vec_embeddings (vec_rowid, embedding) VALUES (?, ?)",
        rusqlite::params![vec_rowid, bytes],
    )
    .unwrap();
    id
}

fn set_vitality_and_access(conn: &Connection, id: &str, vitality: f64, access_count: i64) {
    conn.execute(
        "UPDATE memories SET vitality = ?, access_count = ? WHERE id = ?",
        rusqlite::params![vitality, access_count, id],
    )
    .unwrap();
}

fn column_opt(conn: &Connection, id: &str, name: &str) -> Option<String> {
    conn.query_row(
        &format!("SELECT {} FROM memories WHERE id = ?", name),
        rusqlite::params![id],
        |r| r.get::<_, Option<String>>(0),
    )
    .unwrap()
}

fn column(conn: &Connection, id: &str, name: &str) -> String {
    conn.query_row(
        &format!("SELECT {} FROM memories WHERE id = ?", name),
        rusqlite::params![id],
        |r| r.get::<_, String>(0),
    )
    .unwrap()
}

fn default_input() -> ConsolidateInput {
    ConsolidateInput {
        similarity_threshold: 0.85,
        dry_run: true,
        category: None,
        limit: 500,
        summaries: None,
    }
}

#[test]
fn an_empty_store_reports_no_eligible_memories() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    let report = consolidate(&conn, &default_input()).unwrap();

    assert_eq!(report["clusters_found"], 0);
    assert_eq!(report["message"], "No eligible memories found");
}

#[test]
fn dissimilar_memories_do_not_cluster() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add_with_vector(&conn, "quokkas on Rottnest Island", "general", &[1.0, 0.0]);
    add_with_vector(
        &conn,
        "the deploy window is Tuesdays",
        "general",
        &[0.0, 1.0],
    );

    let report = consolidate(&conn, &default_input()).unwrap();

    assert_eq!(report["clusters_found"], 0);
    assert_eq!(
        report["message"],
        "No similar memories found above threshold"
    );
}

#[test]
fn dry_run_reports_a_cluster_and_each_members_similarity() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = add_with_vector(&conn, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let b = add_with_vector(&conn, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&conn, &a, 0.9, 5);
    set_vitality_and_access(&conn, &b, 0.5, 1);

    let report = consolidate(&conn, &default_input()).unwrap();

    assert_eq!(report["clusters_found"], 1);
    assert_eq!(report["dry_run"], true);
    let cluster = &report["clusters"][0];
    assert_eq!(cluster["canonical"]["id"], a);
    assert_eq!(cluster["cluster_size"], 2);
    assert_eq!(cluster["members"][0]["id"], b);
    assert!(cluster["members"][0]["similarity"].as_f64().unwrap() > 0.9);
}

#[test]
fn dry_run_leaves_the_store_completely_untouched() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = add_with_vector(&conn, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let b = add_with_vector(&conn, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&conn, &a, 0.9, 5);
    set_vitality_and_access(&conn, &b, 0.5, 1);
    let before_a = queries::get_memory_by_id(&conn, &a).unwrap().unwrap();
    let before_b = queries::get_memory_by_id(&conn, &b).unwrap().unwrap();

    // Even with a summary supplied, dry_run must still win over an intent to merge.
    let mut summaries = HashMap::new();
    summaries.insert(a.clone(), "a summary that must not be applied".to_string());
    consolidate(
        &conn,
        &ConsolidateInput {
            dry_run: true,
            summaries: Some(summaries),
            ..default_input()
        },
    )
    .unwrap();

    let after_a = queries::get_memory_by_id(&conn, &a).unwrap().unwrap();
    let after_b = queries::get_memory_by_id(&conn, &b).unwrap().unwrap();
    assert_eq!(after_a.content, before_a.content);
    assert_eq!(after_a.access_count, before_a.access_count);
    assert_eq!(after_b.superseded_by, before_b.superseded_by);
    assert!(after_b.superseded_by.is_none());
}

#[test]
fn category_scoping_excludes_memories_outside_the_requested_category() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Two in "wildlife" that should cluster together...
    add_with_vector(&conn, "quokkas are marsupials", "wildlife", &[1.0, 0.0]);
    add_with_vector(
        &conn,
        "quokkas are small marsupials",
        "wildlife",
        &[0.99, 0.14107],
    );
    // ...and a third, equally similar memory in a different category that scoping must drop.
    add_with_vector(
        &conn,
        "quokkas are cute marsupials",
        "general",
        &[0.98, 0.19867],
    );

    let report = consolidate(
        &conn,
        &ConsolidateInput {
            category: Some("wildlife".to_string()),
            ..default_input()
        },
    )
    .unwrap();

    assert_eq!(report["clusters_found"], 1);
    assert_eq!(report["clusters"][0]["cluster_size"], 2);
}

#[test]
fn limit_caps_the_candidate_pool_fed_into_clustering() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Twelve mutually-identical vectors: without the cap they would all
    // cluster together as one group of 12.
    for i in 0..12 {
        add_with_vector(
            &conn,
            &format!("identical content {i}"),
            "general",
            &[1.0, 0.0],
        );
    }

    let report = consolidate(
        &conn,
        &ConsolidateInput {
            limit: 10, // the reference's floor
            ..default_input()
        },
    )
    .unwrap();

    assert_eq!(report["clusters_found"], 1);
    assert_eq!(
        report["clusters"][0]["cluster_size"], 10,
        "the candidate pool was capped at `limit` before clustering ever ran"
    );
}

#[test]
fn a_cluster_with_no_summary_is_skipped_not_merged() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let a = add_with_vector(&conn, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let b = add_with_vector(&conn, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&conn, &a, 0.9, 5);
    set_vitality_and_access(&conn, &b, 0.5, 1);

    let report = consolidate(
        &conn,
        &ConsolidateInput {
            dry_run: false,
            ..default_input()
        },
    )
    .unwrap();

    assert_eq!(report["clusters_found"], 1);
    assert_eq!(report["clusters_merged"], 0);
    assert_eq!(report["skipped_no_summary"][0], a);
    assert!(column_opt(&conn, &b, "superseded_by").is_none());
}

#[test]
fn merging_combines_content_sums_access_counts_and_supersedes_members() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let canonical = add_with_vector(&conn, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let member = add_with_vector(&conn, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&conn, &canonical, 0.9, 5);
    set_vitality_and_access(&conn, &member, 0.5, 3);

    let mut summaries = HashMap::new();
    summaries.insert(canonical.clone(), "the meeting is at 3pm".to_string());
    let report = consolidate(
        &conn,
        &ConsolidateInput {
            dry_run: false,
            summaries: Some(summaries),
            ..default_input()
        },
    )
    .unwrap();

    assert_eq!(report["clusters_merged"], 1);
    assert_eq!(report["memories_superseded"], 1);
    assert_eq!(report["canonical_ids"][0], canonical);
    assert_eq!(
        column(&conn, &canonical, "content"),
        "the meeting is at 3pm"
    );

    let access_count: i64 = conn
        .query_row(
            "SELECT access_count FROM memories WHERE id = ?",
            [&canonical],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(access_count, 8, "5 (canonical) + 3 (member)");

    assert_eq!(column(&conn, &member, "superseded_by"), canonical);
}

#[test]
fn superseded_members_are_excluded_from_a_later_consolidate_pass() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let canonical = add_with_vector(&conn, "the meeting moved to 3pm", "general", &[1.0, 0.0]);
    let member = add_with_vector(&conn, "meeting is now at 3pm", "general", &[0.99, 0.14107]);
    set_vitality_and_access(&conn, &canonical, 0.9, 5);
    set_vitality_and_access(&conn, &member, 0.5, 3);

    let mut summaries = HashMap::new();
    summaries.insert(canonical.clone(), "the meeting is at 3pm".to_string());
    consolidate(
        &conn,
        &ConsolidateInput {
            dry_run: false,
            summaries: Some(summaries),
            ..default_input()
        },
    )
    .unwrap();

    // The member is now superseded; a second pass should no longer see it
    // (or the cluster it used to form) as a candidate at all.
    let report = consolidate(&conn, &default_input()).unwrap();
    assert_eq!(
        report["clusters_found"], 0,
        "the superseded member must not resurface as a consolidation candidate"
    );
}
