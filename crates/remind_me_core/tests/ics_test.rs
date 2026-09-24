//! Coverage for the reminders calendar feed (gap T1c, issue #118).
//!
//! Folding and escaping get the most attention, because both produce output
//! that reads fine in a text editor and is rejected — or worse, silently
//! misparsed — by a real calendar client. An unescaped comma does not corrupt
//! its own VEVENT; it corrupts every one after it.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::ics::{
    build_ics, escape_ics_text, fold_line, resolve_ics_token, ICS_TOKEN_ENV, ICS_TOKEN_FILE_ENV,
    PRODID, SUMMARY_MAX_CHARS,
};
use remind_me_core::models::Memory;
use std::sync::{Mutex, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn stamp() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05+00:00")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

fn memory(id: &str, content: &str, remind_at: Option<&str>) -> Memory {
    Memory {
        id: id.to_string(),
        content: content.to_string(),
        category: "general".to_string(),
        tags: vec![],
        source: "manual".to_string(),
        metadata: serde_json::json!({}),
        created_at: String::new(),
        updated_at: String::new(),
        capture_id: None,
        subject: None,
        predicate: None,
        object: None,
        superseded_by: None,
        decay_rate: 0.0,
        vitality: 0.0,
        base_weight: 0.0,
        access_count: 0,
        accessed_at: String::new(),
        doc_id: None,
        chunk_index: None,
        remind_at: remind_at.map(str::to_string),
        sensitive: false,
        // Present so a Memory can round-trip to JSON (#198); the calendar
        // feed reads none of them.
        memory_type: None,
        status: None,
        node_id: None,
        client: None,
        source_capture_id: None,
        deleted_at: None,
    }
}

// ---------------------------------------------------------------------------
// Document shape
// ---------------------------------------------------------------------------

#[test]
fn an_empty_feed_is_a_valid_empty_calendar() {
    let doc = build_ics(&[], stamp());

    // Not an error and not an empty string: a calendar client polling a vault
    // with nothing due must get a document it can parse, or it reports the
    // subscription as broken.
    assert!(doc.starts_with("BEGIN:VCALENDAR\r\n"));
    assert!(doc.ends_with("END:VCALENDAR\r\n"));
    assert!(!doc.contains("BEGIN:VEVENT"));
    assert!(doc.contains(&format!("PRODID:{}", PRODID)));
    assert!(doc.contains("VERSION:2.0"));
}

#[test]
fn a_reminder_becomes_a_vevent_with_a_utc_dtstart() {
    let doc = build_ics(
        &[memory(
            "mem_1",
            "renew the passport",
            Some("2026-03-04T05:06:07+00:00"),
        )],
        stamp(),
    );

    assert!(doc.contains("BEGIN:VEVENT"));
    assert!(doc.contains("DTSTART:20260304T050607Z"));
    assert!(doc.contains("DTSTAMP:20260102T030405Z"));
    assert!(doc.contains("SUMMARY:renew the passport"));
    assert!(doc.contains("DESCRIPTION:renew the passport"));
    assert!(doc.contains("END:VEVENT"));
}

#[test]
fn a_non_utc_offset_is_converted_rather_than_copied() {
    let doc = build_ics(
        &[memory("mem_1", "call", Some("2026-03-04T10:00:00+05:00"))],
        stamp(),
    );

    // 10:00+05:00 is 05:00Z. Copying the wall-clock digits and appending `Z`
    // is the classic ICS bug: the event lands five hours late and nothing
    // about the document looks wrong.
    assert!(
        doc.contains("DTSTART:20260304T050000Z"),
        "offset not converted, got:\n{doc}"
    );
}

#[test]
fn every_line_ends_crlf() {
    let doc = build_ics(
        &[memory("mem_1", "x", Some("2026-03-04T05:06:07+00:00"))],
        stamp(),
    );

    // RFC 5545 is CRLF, not LF. Some clients tolerate LF; enough do not that
    // this is worth pinning.
    for line in doc.split("\r\n").filter(|l| !l.is_empty()) {
        assert!(!line.contains('\n'), "bare LF inside {line:?}");
    }
    assert_eq!(doc.matches("\r\n").count(), doc.lines().count() - 1 + 1);
}

#[test]
fn a_memory_with_no_remind_at_is_skipped_not_emitted() {
    let doc = build_ics(
        &[
            memory("mem_none", "no reminder", None),
            memory("mem_1", "has one", Some("2026-03-04T05:06:07+00:00")),
        ],
        stamp(),
    );

    // A VEVENT with no DTSTART would make the *whole document* invalid, not
    // just that entry — so the entry is dropped rather than half-emitted.
    assert_eq!(doc.matches("BEGIN:VEVENT").count(), 1);
    assert!(!doc.contains("mem_none"));
}

#[test]
fn an_unparseable_stored_timestamp_does_not_poison_the_document() {
    let doc = build_ics(
        &[
            memory("mem_bad", "junk timestamp", Some("not a timestamp")),
            memory("mem_ok", "fine", Some("2026-03-04T05:06:07+00:00")),
        ],
        stamp(),
    );

    assert_eq!(doc.matches("BEGIN:VEVENT").count(), 1);
    assert!(doc.contains("mem_ok"));
}

// ---------------------------------------------------------------------------
// UIDs
// ---------------------------------------------------------------------------

#[test]
fn the_uid_is_deterministic_across_calls() {
    let m = memory("mem_1", "x", Some("2026-03-04T05:06:07+00:00"));
    let first = build_ics(std::slice::from_ref(&m), stamp());
    let second = build_ics(std::slice::from_ref(&m), stamp());

    // A subscribing calendar re-fetches on its own schedule. A random UID
    // would make every poll create a fresh duplicate event.
    assert_eq!(first, second);
    assert!(first.contains("UID:mem_1-2026-03-04T05:06:07+00:00@remind-me"));
}

#[test]
fn rescheduling_mints_a_new_uid() {
    let before = build_ics(
        &[memory("mem_1", "x", Some("2026-03-04T05:06:07+00:00"))],
        stamp(),
    );
    let after = build_ics(
        &[memory("mem_1", "x", Some("2026-03-05T05:06:07+00:00"))],
        stamp(),
    );

    // A different time is a genuinely different occurrence, so the calendar
    // should show it as one rather than silently moving the old event.
    assert_ne!(before, after);
    assert!(after.contains("UID:mem_1-2026-03-05T05:06:07+00:00@remind-me"));
}

// ---------------------------------------------------------------------------
// Escaping (RFC 5545 §3.3.11)
// ---------------------------------------------------------------------------

#[test]
fn the_structural_characters_are_escaped() {
    assert_eq!(escape_ics_text("a,b"), "a\\,b");
    assert_eq!(escape_ics_text("a;b"), "a\\;b");
    assert_eq!(escape_ics_text("a\nb"), "a\\nb");
    assert_eq!(escape_ics_text("a\r\nb"), "a\\nb");
    assert_eq!(escape_ics_text("a\rb"), "a\\nb");
}

#[test]
fn backslash_is_escaped_first_so_the_others_are_not_double_escaped() {
    // Escaping the comma first would turn `\` + `,` into `\\,` and then the
    // backslash pass would make it `\\\\,` — the reader then sees a literal
    // backslash followed by an unescaped comma, which ends the value early.
    assert_eq!(escape_ics_text("a\\,b"), "a\\\\\\,b");
}

#[test]
fn a_comma_in_one_reminder_does_not_corrupt_the_next() {
    let doc = build_ics(
        &[
            memory(
                "mem_1",
                "eggs, milk; bread",
                Some("2026-03-04T05:06:07+00:00"),
            ),
            memory("mem_2", "second event", Some("2026-03-05T05:06:07+00:00")),
        ],
        stamp(),
    );

    // The whole point of escaping: a naive parser reading an unescaped `;`
    // treats the rest of the line as a new property and everything after it
    // drifts. Both events have to survive intact.
    assert!(doc.contains("SUMMARY:eggs\\, milk\\; bread"));
    assert_eq!(doc.matches("BEGIN:VEVENT").count(), 2);
    assert!(doc.contains("mem_2"));
}

// ---------------------------------------------------------------------------
// Folding (RFC 5545 §3.1)
// ---------------------------------------------------------------------------

#[test]
fn a_short_line_is_left_alone() {
    assert_eq!(fold_line("SUMMARY:short"), "SUMMARY:short");
}

#[test]
fn a_long_line_folds_at_75_octets_with_a_leading_space() {
    let line = format!("DESCRIPTION:{}", "x".repeat(200));
    let folded = fold_line(&line);

    let physical: Vec<&str> = folded.split("\r\n").collect();
    assert!(physical.len() > 1, "a 212-octet line must fold");
    assert!(physical[0].len() <= 75);
    for continuation in &physical[1..] {
        assert!(
            continuation.starts_with(' '),
            "a continuation must start with the fold marker: {continuation:?}"
        );
        // The marker counts against the line's own 75-octet budget.
        assert!(
            continuation.len() <= 75,
            "continuation too long: {}",
            continuation.len()
        );
    }
    // Unfolding is dropping the CRLF+space, and must give back the original.
    assert_eq!(folded.replace("\r\n ", ""), line);
}

#[test]
fn folding_never_splits_a_multibyte_character() {
    // Every character is 3 bytes, so a naive 75-byte split lands mid-sequence.
    let line = format!("DESCRIPTION:{}", "日".repeat(60));
    let folded = fold_line(&line);

    // If a split had landed inside a character, the chunks would not be valid
    // UTF-8 at all — which in Rust means this function could not have returned.
    // What is worth asserting is that unfolding round-trips exactly.
    assert_eq!(folded.replace("\r\n ", ""), line);
    for physical in folded.split("\r\n") {
        assert!(physical.len() <= 75);
    }
}

#[test]
fn a_long_reminder_is_folded_in_the_document() {
    let doc = build_ics(
        &[memory(
            "mem_1",
            &"a very long reminder ".repeat(20),
            Some("2026-03-04T05:06:07+00:00"),
        )],
        stamp(),
    );

    for line in doc.split("\r\n") {
        assert!(
            line.len() <= 75,
            "unfolded {}-octet line would be rejected by strict clients: {line:?}",
            line.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Summary truncation
// ---------------------------------------------------------------------------

#[test]
fn a_long_summary_is_truncated_but_the_description_keeps_everything() {
    let content = "z".repeat(400);
    let doc = build_ics(
        &[memory("mem_1", &content, Some("2026-03-04T05:06:07+00:00"))],
        stamp(),
    );

    let unfolded = doc.replace("\r\n ", "");
    let summary = unfolded
        .lines()
        .find(|l| l.starts_with("SUMMARY:"))
        .unwrap()
        .trim_end();
    assert_eq!(
        summary.chars().count() - "SUMMARY:".chars().count(),
        SUMMARY_MAX_CHARS
    );
    assert!(summary.ends_with('…'));

    // Nothing is lost, which is what makes truncating the title acceptable.
    assert!(unfolded.contains(&format!("DESCRIPTION:{}", content)));
}

// ---------------------------------------------------------------------------
// The token
// ---------------------------------------------------------------------------

#[test]
fn an_explicit_token_env_var_wins() {
    let _guard = env_lock().lock().unwrap();
    crate::test_env::set_var(ICS_TOKEN_ENV, "explicit-token");

    assert_eq!(resolve_ics_token(), "explicit-token");
    crate::test_env::remove_var(ICS_TOKEN_ENV);
}

#[test]
fn a_generated_token_persists_and_is_reused() {
    let _guard = env_lock().lock().unwrap();
    crate::test_env::remove_var(ICS_TOKEN_ENV);
    let dir = std::env::temp_dir().join(format!("rmm_ics_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("ics_token");
    crate::test_env::set_var(ICS_TOKEN_FILE_ENV, &file);

    let first = resolve_ics_token();
    let second = resolve_ics_token();

    // Regenerating on every call would silently invalidate every calendar
    // subscription the moment the process restarted.
    assert_eq!(first, second);
    assert!(!first.is_empty());
    assert!(file.is_file());

    // Long enough not to be guessable: the token is the entire credential.
    assert!(
        first.len() >= 32,
        "token is only {} chars, and it is the whole credential",
        first.len()
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        // A world-readable token file hands the feed to every local account.
        assert_eq!(mode & 0o077, 0, "token file is group/other readable");
    }

    crate::test_env::remove_var(ICS_TOKEN_FILE_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn deleting_the_token_file_rotates_the_token() {
    let _guard = env_lock().lock().unwrap();
    crate::test_env::remove_var(ICS_TOKEN_ENV);
    let dir = std::env::temp_dir().join(format!("rmm_ics_rot_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("ics_token");
    crate::test_env::set_var(ICS_TOKEN_FILE_ENV, &file);

    let before = resolve_ics_token();
    std::fs::remove_file(&file).unwrap();
    let after = resolve_ics_token();

    // This is the entire revocation story, matching the reference: there is
    // no revocation list and no second valid token, so rotating means every
    // subscribed calendar must be re-pointed.
    assert_ne!(before, after);

    crate::test_env::remove_var(ICS_TOKEN_FILE_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}
