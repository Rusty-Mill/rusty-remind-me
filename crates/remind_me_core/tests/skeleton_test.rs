//! Symbolic compression of a capture (#207).
//!
//! Two properties matter and they pull against each other. The skeleton has to
//! be **cheap** — otherwise it is just a third copy of the transcript — and
//! drill-down has to be **exact**, because a node that returns the wrong lines
//! is worse than no drill-down at all: it reads as authoritative.
//!
//! So the assertions here are about ratios and about specific line content,
//! not about a row existing.

use remind_me_core::capture::auto_capture;
use remind_me_core::skeleton::{node_slice, read_skeleton, write_skeleton, SkeletonError};
use remind_me_core::{AutoCaptureInput, Database, SkeletonWriteInput, SKELETON_CATEGORY};
use rusqlite::Connection;
use std::collections::BTreeMap;

fn db(name: &str) -> Database {
    let dir = std::env::temp_dir().join(format!("rrm_skel_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Database::open(dir.join("memories.db").display().to_string()).unwrap()
}

/// A transcript long enough that reading it is a real cost — 120 turns, each
/// several lines. A four-line fixture would make any skeleton look cheap.
fn long_transcript() -> String {
    (1..=120)
        .map(|turn| {
            format!(
                "User: question number {turn} about the schema\n\
                 Assistant: the answer to {turn} involves the memories table\n\
                 Assistant: and some elaboration on point {turn} that runs on\n"
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

fn capture(conn: &Connection, conversation: &str) -> String {
    auto_capture(
        conn,
        &AutoCaptureInput {
            conversation: conversation.to_string(),
            summary: "A long conversation about the schema".into(),
            title: "Schema session".into(),
            tags: vec![],
            category: "conversation".into(),
            metadata: serde_json::json!({}),
        },
    )
    .unwrap()
    .capture_id
}

fn nodes(pairs: &[(&str, usize, usize)]) -> BTreeMap<String, (usize, usize)> {
    pairs
        .iter()
        .map(|(id, s, e)| (id.to_string(), (*s, *e)))
        .collect()
}

const DIAGRAM: &str = "graph TD\n  n1[Opening] --> n2[Middle]\n  n2 --> n3[Close]";

#[test]
fn a_skeleton_round_trips_with_its_node_map() {
    let db = db("roundtrip");
    let conn = db.conn();
    let capture_id = capture(&conn, &long_transcript());

    write_skeleton(
        &conn,
        &SkeletonWriteInput {
            capture_id: capture_id.clone(),
            mermaid: DIAGRAM.into(),
            nodes: nodes(&[("n1", 1, 30), ("n2", 31, 300), ("n3", 301, 360)]),
        },
    )
    .unwrap();

    let read = read_skeleton(&conn, &capture_id).unwrap().unwrap();
    assert_eq!(read.mermaid, DIAGRAM);
    assert_eq!(read.nodes.get("n2"), Some(&(31, 300)));
    assert_eq!(read.nodes.len(), 3);
}

#[test]
fn a_node_resolves_to_exactly_its_lines() {
    let db = db("slice");
    let conn = db.conn();
    let transcript = long_transcript();
    let capture_id = capture(&conn, &transcript);

    // Turn 2 occupies lines 4..=6: three lines per turn, 1-based.
    write_skeleton(
        &conn,
        &SkeletonWriteInput {
            capture_id: capture_id.clone(),
            mermaid: DIAGRAM.into(),
            nodes: nodes(&[("turn2", 4, 6)]),
        },
    )
    .unwrap();

    let slice = node_slice(&conn, &capture_id, "turn2").unwrap().unwrap();

    assert_eq!((slice.start_line, slice.end_line), (4, 6));
    assert_eq!(slice.content.lines().count(), 3);
    // Exactness is the point: turn 2's lines and nothing from its neighbours.
    assert!(slice.content.contains("question number 2 "));
    assert!(!slice.content.contains("question number 1 "));
    assert!(!slice.content.contains("question number 3 "));
}

#[test]
fn reading_the_skeleton_costs_a_fraction_of_reading_the_dialog() {
    let db = db("cost");
    let conn = db.conn();
    let transcript = long_transcript();
    let capture_id = capture(&conn, &transcript);

    write_skeleton(
        &conn,
        &SkeletonWriteInput {
            capture_id: capture_id.clone(),
            mermaid: DIAGRAM.into(),
            nodes: nodes(&[("n1", 1, 120), ("n2", 121, 240), ("n3", 241, 360)]),
        },
    )
    .unwrap();

    let skeleton = read_skeleton(&conn, &capture_id).unwrap().unwrap();
    let skeleton_cost = serde_json::to_string(&skeleton).unwrap().len();
    let dialog_cost = transcript.len();

    // The whole justification for the feature. Asserted as a ratio rather than
    // an absolute so the fixture can grow without the threshold rotting, and
    // asserted at all so a future change that inlines the transcript into the
    // skeleton fails here instead of quietly costing every caller the saving.
    assert!(
        skeleton_cost * 20 < dialog_cost,
        "skeleton {} bytes vs dialog {} bytes — less than 20x saving",
        skeleton_cost,
        dialog_cost
    );

    // And one drill-down is still far cheaper than the transcript, which is
    // what makes the two-step read worth doing rather than just fetching it.
    let slice = node_slice(&conn, &capture_id, "n2").unwrap().unwrap();
    assert!(slice.content.len() * 2 < dialog_cost);
}

#[test]
fn a_range_past_the_end_of_the_dialog_is_refused_at_write_time() {
    let db = db("badrange");
    let conn = db.conn();
    let capture_id = capture(&conn, "one\ntwo\nthree");

    let err = write_skeleton(
        &conn,
        &SkeletonWriteInput {
            capture_id: capture_id.clone(),
            mermaid: DIAGRAM.into(),
            nodes: nodes(&[("n1", 1, 99)]),
        },
    )
    .unwrap_err();

    match err {
        SkeletonError::BadRange { node, lines, .. } => {
            assert_eq!(node, "n1");
            assert_eq!(lines, 3);
        }
        other => panic!("expected BadRange, got {:?}", other),
    }

    // Refused means nothing was stored — a rejected write must not leave a
    // half-skeleton that later reads as authoritative.
    assert!(read_skeleton(&conn, &capture_id).unwrap().is_none());
}

#[test]
fn zero_based_ranges_fail_loudly_rather_than_sliding_by_one() {
    let db = db("zerobased");
    let conn = db.conn();
    let capture_id = capture(&conn, "one\ntwo\nthree");

    // A model that emitted 0-based offsets would otherwise return one line too
    // many, forever, and look plausible doing it.
    let err = write_skeleton(
        &conn,
        &SkeletonWriteInput {
            capture_id,
            mermaid: DIAGRAM.into(),
            nodes: nodes(&[("n1", 0, 2)]),
        },
    )
    .unwrap_err();

    assert!(matches!(err, SkeletonError::BadRange { .. }));
}

#[test]
fn writing_again_replaces_rather_than_accumulates() {
    let db = db("replace");
    let conn = db.conn();
    let capture_id = capture(&conn, "one\ntwo\nthree\nfour");

    for mermaid in ["graph TD\n  a[First]", "graph TD\n  b[Second]"] {
        write_skeleton(
            &conn,
            &SkeletonWriteInput {
                capture_id: capture_id.clone(),
                mermaid: mermaid.into(),
                nodes: nodes(&[("n1", 1, 2)]),
            },
        )
        .unwrap();
    }

    let stored: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE capture_id = ? AND category = ?",
            rusqlite::params![capture_id, SKELETON_CATEGORY],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, 1, "a capture has one shape, not a history of them");
    assert_eq!(
        read_skeleton(&conn, &capture_id).unwrap().unwrap().mermaid,
        "graph TD\n  b[Second]"
    );
}

#[test]
fn an_unknown_node_is_an_empty_answer_not_an_error() {
    let db = db("unknown");
    let conn = db.conn();
    let capture_id = capture(&conn, "one\ntwo\nthree");

    write_skeleton(
        &conn,
        &SkeletonWriteInput {
            capture_id: capture_id.clone(),
            mermaid: DIAGRAM.into(),
            nodes: nodes(&[("n1", 1, 2)]),
        },
    )
    .unwrap();

    assert!(node_slice(&conn, &capture_id, "nope").unwrap().is_none());
    assert!(read_skeleton(&conn, "cap_nonexistent").unwrap().is_none());
}

#[test]
fn a_skeleton_is_not_offered_to_the_annotation_backlog() {
    let db = db("extract");
    let conn = db.conn();
    let capture_id = capture(&conn, "one\ntwo\nthree");
    write_skeleton(
        &conn,
        &SkeletonWriteInput {
            capture_id,
            mermaid: DIAGRAM.into(),
            nodes: nodes(&[("n1", 1, 2)]),
        },
    )
    .unwrap();

    // Mermaid source has no triple in it. Offering it would spend a model call
    // to discover that, the same reason `dialog` is excluded.
    let batch = remind_me_core::db::queries::unannotated_batch(
        &conn,
        &remind_me_core::ExtractBatchInput { batch_size: 50 },
    )
    .unwrap();
    assert!(
        batch
            .memories
            .iter()
            .all(|m| m.category != SKELETON_CATEGORY),
        "skeleton leaked into the extraction backlog"
    );
}

#[test]
fn a_skeleton_needs_at_least_one_node() {
    let db = db("nonodes");
    let conn = db.conn();
    let capture_id = capture(&conn, "one\ntwo");

    let err = write_skeleton(
        &conn,
        &SkeletonWriteInput {
            capture_id,
            mermaid: DIAGRAM.into(),
            nodes: BTreeMap::new(),
        },
    )
    .unwrap_err();

    assert!(matches!(err, SkeletonError::NoNodes));
}
