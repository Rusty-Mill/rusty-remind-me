//! Coverage for `remind_me_server_status`.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::backup::create_backup;
use remind_me_core::db::queries;
use remind_me_core::db::schema::SCHEMA_VERSION;
use remind_me_core::embedder::EMBEDDING_BACKEND_ENV;
use remind_me_core::status::{server_status, SubsystemStatus};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::sync::Mutex;

/// Held by every test in this file that reads `report.embeddings`: that
/// field now reflects `REMIND_ME_EMBEDDING_BACKEND`, a process-global env
/// var, so a test asserting the unset (default) case must not race a test
/// that sets it to `"ollama"` — same convention as `sync_test.rs`'s
/// `ENV_LOCK`.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn add(conn: &Connection, content: &str) {
    queries::add_memory(
        conn,
        MemoryAddInput {
            sensitive: false,
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap();
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rrm_status_{}_{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn is_missing(status: &SubsystemStatus) -> bool {
    matches!(status, SubsystemStatus::NotImplemented { .. })
}

#[test]
fn a_fresh_store_reports_a_current_schema() {
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();

    assert_eq!(report.schema_version, SCHEMA_VERSION);
    assert_eq!(report.expected_schema_version, SCHEMA_VERSION);
    assert!(report.schema_current);
    assert_eq!(report.memory_count, 0);
}

/// #270: `scheduler.running` must say `false` when no loop has been started
/// for this process, not merely omit the field or default to `true`.
/// `background_loop_liveness_test.rs` covers the loop actually running;
/// this is the shape `server_status` reports it in.
#[test]
fn a_fresh_process_reports_the_scheduler_as_not_running() {
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();

    assert!(!report.scheduler.running);
    assert!(report.scheduler.poll_interval_seconds > 0);
}

#[test]
fn a_stale_schema_version_is_reported_as_not_current() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    conn.execute_batch("PRAGMA user_version = 3;").unwrap();

    let report = server_status(&conn).unwrap();

    // A version mismatch is what makes a database unreadable to `remind_me`,
    // so it is surfaced rather than left for a caller to infer.
    assert_eq!(report.schema_version, 3);
    assert!(!report.schema_current);
}

#[test]
fn deleted_memories_are_not_counted() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "kept");
    add(&conn, "going");
    let doomed: String = conn
        .query_row("SELECT id FROM memories WHERE content = 'going'", [], |r| {
            r.get(0)
        })
        .unwrap();
    queries::delete_memory(&conn, &doomed).unwrap();

    assert_eq!(server_status(&conn).unwrap().memory_count, 1);
}

#[test]
fn an_in_memory_database_has_no_path_or_backup_directory() {
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();

    assert!(report.database_path.is_none());
    assert!(report.database_exists, "it exists, it just has no file");
    assert!(report.database_bytes.is_none());
    // Absent rather than empty: there is no directory to hold backups.
    assert!(report.backup_dir.is_none());
    assert_eq!(report.backup_count, 0);
}

#[test]
fn an_on_disk_database_reports_its_path_and_size() {
    let dir = scratch("ondisk");
    let path = dir.join("memories.db");
    let db = Database::open(&path).unwrap();
    let conn = db.conn();
    add(&conn, "a memory");

    let report = server_status(&conn).unwrap();

    assert_eq!(
        report.database_path.as_deref(),
        Some(path.display().to_string().as_str())
    );
    assert!(report.database_exists);
    assert!(report.database_bytes.unwrap() > 0);
    assert!(report.backup_dir.is_some());

    // `conn` is a `MutexGuard` borrowed from `db` -- dropping it only
    // releases the lock. The actual `rusqlite::Connection` (and its open
    // handle on `path`) lives inside `db` itself, so `db` has to go too
    // before Windows will allow deleting the directory that holds it.
    drop(conn);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn backups_are_inventoried_newest_first() {
    let dir = scratch("backups");
    let path = dir.join("memories.db");
    let db = Database::open(&path).unwrap();
    let conn = db.conn();
    add(&conn, "a memory");

    assert_eq!(server_status(&conn).unwrap().backup_count, 0);
    create_backup(&conn, "first").unwrap();
    create_backup(&conn, "second").unwrap();

    let report = server_status(&conn).unwrap();
    assert_eq!(report.backup_count, 2);
    let latest = report.latest_backup.unwrap();
    assert!(
        latest.filename.contains("second"),
        "newest first, got {}",
        latest.filename
    );

    // See the matching comment in `an_on_disk_database_reports_its_path_and_size`.
    drop(conn);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn absent_subsystems_are_named_with_a_reason() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();

    assert!(matches!(report.mcp, SubsystemStatus::Active));
    // `dashboard` and `sync` are genuinely cross-process (a separate
    // `rusty-remind-me api` process, and a worker the MCP server owns) --
    // `server_status` alone can never answer for either, which is why both
    // stay reported as not-implemented with a reason here (the MCP dispatch
    // layer overrides both with live state; see `remind_me_mcp`'s
    // `remind_me_server_status` arm and `crates/remind_me_mcp/tests`).
    for status in [&report.dashboard, &report.sync] {
        assert!(is_missing(status));
        if let SubsystemStatus::NotImplemented { reason } = status {
            assert!(!reason.is_empty(), "a reason must say something");
        }
    }
    // `embeddings`, unlike those two, is config read the same way in every
    // process -- with no backend configured, `server_status` itself already
    // reports it accurately, no override needed to prove this case.
    assert!(is_missing(&report.embeddings));
}

#[test]
fn embeddings_status_is_active_when_a_backend_is_configured() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::set_var(EMBEDDING_BACKEND_ENV, "ollama");
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();

    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    assert!(
        matches!(report.embeddings, SubsystemStatus::Active),
        "expected Active, got {:?}",
        report.embeddings
    );
}

#[test]
fn embeddings_status_is_not_implemented_without_a_configured_backend() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();

    assert!(is_missing(&report.embeddings));
    if let SubsystemStatus::NotImplemented { reason } = &report.embeddings {
        assert!(reason.contains(EMBEDDING_BACKEND_ENV));
    }
}

#[test]
fn the_report_serialises_with_the_subsystem_state_tagged() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::test_env::remove_var(EMBEDDING_BACKEND_ENV);
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();
    let json = serde_json::to_value(&report).unwrap();

    assert_eq!(json["mcp"]["state"], "active");
    assert_eq!(json["embeddings"]["state"], "not_implemented");
    assert!(json["embeddings"]["reason"].is_string());
    // The watcher exists now, so it reports its own state rather than being
    // listed as an absent subsystem.
    assert_eq!(json["watcher"]["enabled"], false);
    assert!(json["watcher"]["hint"].is_string());
    assert_eq!(json["schema_current"], true);
}

#[test]
fn the_report_names_the_build_that_produced_it() {
    // Gap T10, issue #104. A stale install after a failed self-update explains
    // more odd behaviour than anything else this report covers, and it is the
    // one fact a calling session has no other way to observe — so the value has
    // to be the real compiled-in version, not a placeholder that would make a
    // stale build look current.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = Database::open_in_memory().unwrap();

    let report = server_status(&db.conn()).unwrap();

    assert_eq!(report.version, env!("CARGO_PKG_VERSION"));
    assert!(
        !report.version.is_empty(),
        "an empty version reads as 'no information' rather than 'up to date'"
    );

    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
}
