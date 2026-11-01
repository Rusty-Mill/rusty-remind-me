//! End-to-end coverage for `entity-duplicates` (card #65): the real binary
//! against a real database, so the dispatch arm itself is exercised.

use std::path::{Path, PathBuf};
use std::process::Command;

fn run(db: &Path, args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_rusty-remind-me"))
        .args(args)
        .env("REMIND_ME_DB_PATH", db)
        // In this process: a daemon started here would outlive the test.
        .env("REMIND_ME_DAEMON", "0")
        .output()
        .expect("the CLI binary should be runnable");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

fn scratch_db(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rrm-dupes-test-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir.join("remind_me.db")
}

#[test]
fn an_empty_store_has_no_duplicates() {
    let db = scratch_db("empty");
    let (stdout, stderr, ok) = run(&db, &["entity-duplicates"]);
    assert!(ok, "stderr: {stderr}");
    assert_eq!(stdout, "No likely duplicate entities.\n");
    let (stdout, _, ok) = run(&db, &["entity-duplicates", "--json"]);
    assert!(ok);
    assert_eq!(stdout.trim(), "[]");
}

#[test]
fn spellings_of_one_name_are_listed_and_nothing_is_merged() {
    let db = scratch_db("spellings");
    for name in ["rusty_mux", "rusty-mux"] {
        let (_, stderr, ok) = run(&db, &["entity", name]);
        assert!(ok, "stderr: {stderr}");
    }
    let (stdout, stderr, ok) = run(&db, &["entity-duplicates", "--json"]);
    assert!(ok, "stderr: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let pairs = report.as_array().unwrap();
    assert_eq!(pairs.len(), 1, "{stdout}");
    assert_eq!(pairs[0]["reason"], "same_key");

    let (text, _, _) = run(&db, &["entity-duplicates"]);
    assert!(
        text.contains("rusty_mux") && text.contains("rusty-mux"),
        "{text}"
    );
    // Reporting is read-only: both entities are still there.
    let (again, _, _) = run(&db, &["entity-duplicates", "--json"]);
    assert_eq!(again, stdout);
}

#[test]
fn an_unknown_argument_is_refused() {
    let db = scratch_db("bad-arg");
    let (_, stderr, ok) = run(&db, &["entity-duplicates", "--yaml"]);
    assert!(!ok);
    assert!(
        stderr.contains("Usage: rusty-remind-me entity-duplicates"),
        "{stderr}"
    );
}
