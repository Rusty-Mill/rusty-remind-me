//! `capture-transcript --format messages` (ADR-0028 §5): a transcript given as
//! a list of `{role, text}` is captured like a JSONL one: one capture per
//! session, replaced in place, labelled as a machine capture.

use remind_me_core::session_ops::capture_messages;
use remind_me_core::Database;

fn db(name: &str) -> Database {
    let dir = std::env::temp_dir().join(format!("rrm_msgs_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Database::open(dir.join("memories.db").display().to_string()).unwrap()
}

#[test]
fn a_message_list_is_captured_once_per_session_and_replaced_in_place() {
    let db = db("once");
    let store = db.store();
    let first = r#"[{"role":"user","text":"what is a quokka"},{"role":"assistant","text":"a small wallaby"}]"#;
    let made = capture_messages(&store, first, "hermes-s1", None, Some("end")).unwrap();
    assert_eq!(made["messages"], 2);

    let grown = r#"[{"role":"user","text":"what is a quokka"},{"role":"assistant","text":"a small wallaby"},{"role":"user","text":"thanks"},{"role":"tool","text":"  "}]"#;
    let again = capture_messages(&store, grown, "hermes-s1", None, Some("end")).unwrap();
    assert_eq!(
        again["capture_id"], made["capture_id"],
        "replaced, not added"
    );
    assert_eq!(again["messages"], 3, "blank text is dropped");

    let dialog =
        remind_me_core::db::queries::get_memory_by_id(&store, again["dialog_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
    // A grown transcript adds the new messages after the first dialog row.
    let rows = remind_me_core::episodes::session_memories(&store, "hermes-s1").unwrap();
    assert!(rows.iter().any(|m| m.content.contains("thanks")));
    assert_eq!(dialog.written_by, "hook");
    assert_eq!(dialog.capture_method, "auto");
    assert_eq!(dialog.session_id.as_deref(), Some("hermes-s1"));
}

#[test]
fn a_bad_list_stores_nothing() {
    let db = db("bad");
    let store = db.store();
    for input in [
        "not json",
        "[]",
        r#"[{"role":"user","text":"   "}]"#,
        r#"[{"role":" ","text":"x"}]"#,
        r#"[{"role":"user","content":"x"}]"#,
    ] {
        assert!(
            capture_messages(&store, input, "s", None, None).is_err(),
            "{input} accepted"
        );
    }
    assert!(capture_messages(&store, r#"[{"role":"user","text":"x"}]"#, " ", None, None).is_err());
    assert_eq!(
        remind_me_core::testing::memory_ids(&store).unwrap().len(),
        0
    );
}

#[test]
fn distinct_clients_with_one_session_id_keep_their_own_captures() {
    // The Hermes plugin keys a capture `<client>:<session>` (D139).
    let db = db("clients");
    let store = db.store();
    let list = r#"[{"role":"user","text":"what is a quokka"}]"#;
    let jarvis = capture_messages(&store, list, "hermes-jarvis:s1", None, Some("end")).unwrap();
    let gateway = capture_messages(&store, list, "hermes-gw:s1", None, Some("end")).unwrap();
    assert_ne!(jarvis["capture_id"], gateway["capture_id"]);

    let grown = r#"[{"role":"user","text":"what is a quokka"},{"role":"assistant","text":"a small wallaby"}]"#;
    let again = capture_messages(&store, grown, "hermes-jarvis:s1", None, Some("end")).unwrap();
    assert_eq!(again["capture_id"], jarvis["capture_id"]);
    let gw_rows = remind_me_core::episodes::session_memories(&store, "hermes-gw:s1").unwrap();
    assert!(gw_rows.iter().all(|m| !m.content.contains("wallaby")));
}
