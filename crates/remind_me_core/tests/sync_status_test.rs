//! Coverage for `remind_me_sync_status` / `remind_me_sync_repair` (gap T2a,
//! issue #114).
//!
//! Its own test binary: enabling sync means three process-wide env vars, and
//! half of these tests need it off.
//!
//! The distinction under test throughout is the one the v20 migration added
//! the `_at` columns for — a quiet-but-healthy remote versus a wedged one.
//! Reading liveness off the content cursors conflates them, and the conflation
//! is invisible until a peer actually stalls.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::db::queries;
use remind_me_core::sync::{sync_repair, sync_status, HUB_URL_ENV, NODE_ID_ENV, SYNC_SECRET_ENV};
use remind_me_core::{Database, DrainVerdict, MemoryAddInput, SyncStatus};
use rusqlite::Connection;
use std::sync::Mutex;

/// The sync switch is three process-wide env vars and half these tests need it
/// off, so every test serialises on this. Without it they race and the failure
/// reads as a logic bug in the drain verdict rather than as a test-harness
/// problem — which is exactly how it first presented.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const EPOCH: &str = "1970-01-01T00:00:00+00:00";

fn enable_sync() {
    crate::test_env::set_var(NODE_ID_ENV, "node-status-test");
    crate::test_env::set_var(HUB_URL_ENV, "http://hub.example");
    crate::test_env::set_var(SYNC_SECRET_ENV, "shh");
}

fn disable_sync() {
    crate::test_env::remove_var(NODE_ID_ENV);
    crate::test_env::remove_var(HUB_URL_ENV);
    crate::test_env::remove_var(SYNC_SECRET_ENV);
}

fn add(conn: &Connection, content: &str) {
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive: false,
        },
    )
    .unwrap();
}

fn remote(conn: &Connection, id: &str, attempt: &str, push: &str, pull: &str) {
    conn.execute(
        "INSERT INTO sync_log (remote_id, last_pull, last_push, last_pull_id,
                               last_attempt_at, last_push_at, last_pull_at)
         VALUES (?, ?, ?, 'cursor-abc', ?, ?, ?)",
        rusqlite::params![
            id,
            "2026-01-01T00:00:00+00:00",
            "2026-01-01T00:00:00+00:00",
            attempt,
            push,
            pull
        ],
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Disabled
// ---------------------------------------------------------------------------

#[test]
fn a_disabled_node_names_the_missing_variables() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    disable_sync();
    let db = Database::open_in_memory().unwrap();

    let status = sync_status(&db.conn()).unwrap();

    // The caller is asking because they expected sync to be on. "Sync is off"
    // sends them looking; naming the variables answers the question.
    let SyncStatus::Disabled { missing, hint } = status else {
        panic!("expected disabled");
    };
    assert_eq!(missing.len(), 3);
    assert!(missing.iter().any(|m| m.contains("NODE_ID")));
    assert!(hint.contains("REMIND_ME_NODE_ID"));
}

// ---------------------------------------------------------------------------
// Liveness: the whole point of the _at columns
// ---------------------------------------------------------------------------

#[test]
fn a_never_contacted_remote_is_distinguishable_from_a_failing_one() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // Never contacted: every timestamp still at the epoch default.
    remote(&conn, "peer-never", EPOCH, EPOCH, EPOCH);
    // Contacted recently, but nothing has succeeded since — attempt advanced,
    // push and pull did not. This is a wedged remote.
    remote(
        &conn,
        "peer-failing",
        "2026-08-03T04:00:00+00:00",
        "2026-07-01T00:00:00+00:00",
        "2026-07-01T00:00:00+00:00",
    );

    let SyncStatus::Enabled { remotes, .. } = sync_status(&conn).unwrap() else {
        panic!("expected enabled");
    };

    let never = remotes
        .iter()
        .find(|r| r.remote_id == "peer-never")
        .unwrap();
    let failing = remotes
        .iter()
        .find(|r| r.remote_id == "peer-failing")
        .unwrap();

    // The epoch default is not NULL, so "never" has to be recognised by value
    // or it reads as a very stale timestamp — which is a different diagnosis.
    assert!(!never.ever_contacted);
    assert!(failing.ever_contacted);
    assert_ne!(failing.last_attempt_at, failing.last_push_at);
    disable_sync();
}

#[test]
fn liveness_comes_from_the_at_columns_not_the_cursors() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    // A quiet but perfectly healthy remote: contacted moments ago, but its
    // content cursors have not moved because there was nothing new to send.
    remote(
        &conn,
        "peer-quiet",
        "2026-08-03T04:00:00+00:00",
        "2026-08-03T04:00:00+00:00",
        "2026-08-03T04:00:00+00:00",
    );

    let SyncStatus::Enabled { remotes, .. } = sync_status(&conn).unwrap() else {
        panic!("expected enabled");
    };
    let quiet = &remotes[0];

    // Reading liveness off `last_pull` would report this remote as stuck in
    // January. The reported timestamps must be the contact clocks.
    assert_eq!(quiet.last_pull_at, "2026-08-03T04:00:00+00:00");
    assert_eq!(quiet.last_push_at, "2026-08-03T04:00:00+00:00");
    assert!(quiet.ever_contacted);
    disable_sync();
}

#[test]
fn a_namespaced_pull_cursor_reports_the_same_pending_as_its_base_remote() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "queued for hub");

    // Mark the outbox row delivered to the base remote "hub" -- the same
    // sync_sends row a real push_outbox cycle would leave behind.
    conn.execute(
        "INSERT INTO sync_sends (remote_id, outbox_id, sent_at)
         SELECT 'hub', id, '2026-08-09T00:00:00+00:00' FROM sync_outbox",
        [],
    )
    .unwrap();

    // A namespaced pull-only cursor for the same destination: entities never
    // get an independent sync_sends row keyed to "hub#entities" -- the
    // memories/entities/links outbox is one queue, pushed once under "hub".
    remote(
        &conn,
        "hub#entities",
        "2026-08-09T00:00:00+00:00",
        EPOCH,
        "2026-08-09T00:00:00+00:00",
    );

    let SyncStatus::Enabled { remotes, .. } = sync_status(&conn).unwrap() else {
        panic!("expected enabled");
    };
    let entities = remotes
        .iter()
        .find(|r| r.remote_id == "hub#entities")
        .unwrap();

    // Before the fix this always read back the *entire* outbox -- a literal
    // lookup against "hub#entities", which sync_sends never records under --
    // reporting a permanent backlog no push cycle could ever drain.
    assert_eq!(entities.pending, 0);
    disable_sync();
}

#[test]
fn a_graph_cursor_row_reports_the_base_remotes_push_state_not_its_own() {
    // `hub#entities`/`hub#links`/`hub#entity_relations` are pull-only cursor
    // rows -- `sync_with_remote` drains the whole outbox in one push keyed by
    // the bare `"hub"`, so `sync_sends`/`last_push_at` are never written
    // under a cursor's own `#`-suffixed key. Reading those rows the same way
    // as a real push target reported a permanently-full backlog and an
    // eternal epoch `last_push_at` for graph data that was, in fact, pushed
    // and draining fine alongside `memories`.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "m1");
    // Mark the one outbox row sent to the base remote, "hub" -- as a real
    // push cycle would.
    conn.execute(
        "INSERT INTO sync_sends (remote_id, outbox_id, sent_at)
         SELECT 'hub', id, '2026-08-03T04:00:00+00:00' FROM sync_outbox",
        [],
    )
    .unwrap();
    remote(
        &conn,
        "hub",
        "2026-08-03T04:00:00+00:00",
        "2026-08-03T04:00:00+00:00",
        "2026-08-03T04:00:00+00:00",
    );
    // The cursor row: attempted and pulled recently, but its own
    // `last_push_at` sits at the epoch default forever, since nothing ever
    // writes it.
    remote(
        &conn,
        "hub#entities",
        "2026-08-03T05:00:00+00:00",
        EPOCH,
        "2026-08-03T05:00:00+00:00",
    );

    let SyncStatus::Enabled { remotes, .. } = sync_status(&conn).unwrap() else {
        panic!("expected enabled");
    };
    let hub = remotes.iter().find(|r| r.remote_id == "hub").unwrap();
    let entities = remotes
        .iter()
        .find(|r| r.remote_id == "hub#entities")
        .unwrap();

    assert_eq!(hub.pending, 0);
    // Falls back to the base remote's real push state instead of reporting
    // a full, permanently-stuck backlog.
    assert_eq!(entities.pending, 0);
    assert_eq!(entities.last_push_at, "2026-08-03T04:00:00+00:00");
    // Its own attempt/pull clocks are untouched -- only push state borrows
    // from the base remote.
    assert_eq!(entities.last_attempt_at, "2026-08-03T05:00:00+00:00");
    assert_eq!(entities.last_pull_at, "2026-08-03T05:00:00+00:00");
    assert!(entities.ever_contacted);
    disable_sync();
}
// ---------------------------------------------------------------------------
// Outbox and the drain verdict
// ---------------------------------------------------------------------------

#[test]
fn an_empty_outbox_is_idle_not_unknown() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();

    let SyncStatus::Enabled { outbox, .. } = sync_status(&db.conn()).unwrap() else {
        panic!("expected enabled");
    };

    // Nothing pending needs no baseline to interpret.
    assert_eq!(outbox.pending, 0);
    assert_eq!(outbox.drain, DrainVerdict::Idle);
    disable_sync();
}

#[test]
fn the_first_call_with_a_backlog_admits_it_cannot_tell_yet() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "queued");

    let SyncStatus::Enabled { outbox, .. } = sync_status(&conn).unwrap() else {
        panic!("expected enabled");
    };

    // A direction needs two observations. Guessing from one would make a
    // healthy push look stalled at exactly the moment someone checks.
    assert!(outbox.pending > 0);
    assert_eq!(outbox.drain, DrainVerdict::Unknown);
    assert!(outbox.oldest_pending.is_some());
    disable_sync();
}

#[test]
fn a_second_call_with_an_unchanged_backlog_reports_stalled() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "queued");

    sync_status(&conn).unwrap();
    let SyncStatus::Enabled { outbox, .. } = sync_status(&conn).unwrap() else {
        panic!("expected enabled");
    };

    // This is the verdict a pending count alone cannot give: the same number
    // twice means nothing is moving.
    assert_eq!(outbox.drain, DrainVerdict::Stalled);
    disable_sync();
}

#[test]
fn a_growing_backlog_is_reported_as_growing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "first");
    sync_status(&conn).unwrap();
    add(&conn, "second");

    let SyncStatus::Enabled { outbox, .. } = sync_status(&conn).unwrap() else {
        panic!("expected enabled");
    };

    assert_eq!(outbox.drain, DrainVerdict::Growing);
    disable_sync();
}

#[test]
fn a_shrinking_backlog_is_reported_as_draining() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "first");
    add(&conn, "second");
    sync_status(&conn).unwrap();
    conn.execute(
        "INSERT INTO sync_sends (remote_id, outbox_id, sent_at)
         SELECT 'hub', id, '2026-08-03T04:00:00+00:00' FROM sync_outbox LIMIT 1",
        [],
    )
    .unwrap();

    let SyncStatus::Enabled { outbox, .. } = sync_status(&conn).unwrap() else {
        panic!("expected enabled");
    };

    assert_eq!(outbox.drain, DrainVerdict::Draining);
    assert!(outbox.per_minute.is_some_and(|r| r < 0.0));
    disable_sync();
}

#[test]
fn tombstones_are_counted_and_split_by_compactability() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "recent tombstone");
    add(&conn, "old tombstone");
    conn.execute(
        "UPDATE memories SET deleted_at = ? WHERE content = 'recent tombstone'",
        [chrono::Utc::now().to_rfc3339()],
    )
    .unwrap();
    conn.execute(
        "UPDATE memories SET deleted_at = '2020-01-01T00:00:00+00:00'
          WHERE content = 'old tombstone'",
        [],
    )
    .unwrap();

    let SyncStatus::Enabled { tombstones, .. } = sync_status(&conn).unwrap() else {
        panic!("expected enabled");
    };

    // Both numbers matter: total is disk you will not get back yet,
    // compactable is disk you could get back now.
    assert_eq!(tombstones.total, 2);
    assert_eq!(tombstones.compactable_now, 1);
    disable_sync();
}

// ---------------------------------------------------------------------------
// Repair
// ---------------------------------------------------------------------------

#[test]
fn repair_resets_the_cursor_and_leaves_the_contact_clocks_alone() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    remote(
        &conn,
        "hub",
        "2026-08-03T04:00:00+00:00",
        "2026-08-03T04:00:00+00:00",
        "2026-08-03T04:00:00+00:00",
    );

    assert!(sync_repair(&conn, "hub").unwrap());

    let (last_pull, cursor_id, attempt): (String, String, String) = conn
        .query_row(
            "SELECT last_pull, last_pull_id, last_attempt_at FROM sync_log WHERE remote_id = 'hub'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();

    // The cursor goes back to the epoch so history is re-pulled...
    assert_eq!(last_pull, EPOCH);
    assert_eq!(cursor_id, "");
    // ...but the contact clock records what actually happened. Rewriting it to
    // force a re-pull would destroy the evidence you were reading when you
    // decided a repair was needed.
    assert_eq!(attempt, "2026-08-03T04:00:00+00:00");
    disable_sync();
}

#[test]
fn repairing_an_unknown_remote_reports_that_rather_than_succeeding() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    enable_sync();
    let db = Database::open_in_memory().unwrap();

    // A remote never contacted has nothing to repair. Reporting success would
    // send the caller waiting for a re-pull that is not coming.
    assert!(!sync_repair(&db.conn(), "never-seen").unwrap());
    disable_sync();
}
