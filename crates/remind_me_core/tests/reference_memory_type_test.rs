//! The `reference` memory_type and its v28 → v29 refiling (reference issue #220).
//!
//! The seven older types all describe a *claim*. Bulk-imported file contents —
//! source, diagrams, doc fragments — assert nothing, so they were filed as
//! `fact` for want of anywhere better, which made `fact`-filtered views a
//! mixture of real assertions and pasted-in source.
//!
//! This matters more here than in the reference, because the two share a
//! database by design (ARCHITECTURE.md Tenet 3). Before this, a `reference`
//! row written by `remind_me` and read by this crate fell through the decay
//! table's catch-all and aged at 0.10 — more than three times the intended
//! rate — silently, in a store both sides are supposed to read identically.

use remind_me_core::db::migrations::SCHEMA_VERSION;
use remind_me_core::vitality::{get_decay_rate, get_type_prior, REFERENCE_DECAY_RATE};
use remind_me_core::Database;
use rusqlite::Connection;

/// A temporary directory that cleans up after itself.
struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "rrm-reference-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&base).unwrap();
        Self(base)
    }
    fn db_path(&self) -> std::path::PathBuf {
        self.0.join("test.db")
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build a database stamped at `version` holding `rows` of
/// `(id, source, memory_type, decay_rate)`, then open it through `Database`
/// so the reconciliation — and the refiling — runs.
fn open_with_rows(
    path: &std::path::Path,
    version: i32,
    rows: &[(&str, &str, &str, f64)],
) -> Database {
    {
        let conn = Connection::open(path).unwrap();
        // Minimal shape: the reconciler adds every other column.
        conn.execute_batch(
            "CREATE TABLE memories (
                 id TEXT PRIMARY KEY,
                 content TEXT NOT NULL,
                 source TEXT NOT NULL DEFAULT 'manual',
                 memory_type TEXT NOT NULL DEFAULT 'unclassified',
                 decay_rate REAL NOT NULL DEFAULT 0.05,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 deleted_at TEXT
             );",
        )
        .unwrap();
        for (id, source, memory_type, decay_rate) in rows {
            conn.execute(
                "INSERT INTO memories
                     (id, content, source, memory_type, decay_rate, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00')",
                rusqlite::params![id, format!("body of {id}"), source, memory_type, decay_rate],
            )
            .unwrap();
        }
        conn.execute_batch(&format!("PRAGMA user_version = {version};"))
            .unwrap();
    }
    Database::open(path).unwrap()
}

fn row(conn: &Connection, id: &str) -> (String, f64, String) {
    conn.query_row(
        "SELECT memory_type, decay_rate, updated_at FROM memories WHERE id = ?",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// The type itself
// ---------------------------------------------------------------------------

#[test]
fn reference_has_its_own_decay_rate_and_prior_rather_than_the_fallback() {
    // The shared-database failure: without an arm of its own, `reference`
    // lands on the catch-all and ages at 0.10 while `remind_me` ages the same
    // row at 0.03.
    assert_eq!(get_decay_rate("reference"), 0.03);
    assert_eq!(get_type_prior("reference"), 0.95);

    let fallback_decay = get_decay_rate("something_nobody_defined");
    let fallback_prior = get_type_prior("something_nobody_defined");
    assert_ne!(
        get_decay_rate("reference"),
        fallback_decay,
        "reference must not be resolving through the catch-all"
    );
    assert_ne!(get_type_prior("reference"), fallback_prior);
}

#[test]
fn reference_decays_slower_than_fact_but_faster_than_decision() {
    // The ordering is the claim, not the literals: time alone does not stale a
    // snippet the way it stales a claim about current state, but the file a
    // reference mirrors changes more often than a decision is reversed.
    assert!(get_decay_rate("reference") < get_decay_rate("fact"));
    assert!(get_decay_rate("reference") > get_decay_rate("decision"));
}

#[test]
fn the_migration_constant_is_the_canonical_one() {
    // The reference has to duplicate this constant (its `vitality` imports
    // `db`, so importing back is a cycle) and guards the copy with a drift
    // test. Here there is one definition, so this asserts the wiring rather
    // than the absence of drift.
    assert_eq!(get_decay_rate("reference"), REFERENCE_DECAY_RATE);
}

// ---------------------------------------------------------------------------
// v28 -> v29 refiling
// ---------------------------------------------------------------------------

#[test]
fn v29_refiles_mempalace_facts_as_reference() {
    let tmp = TmpDir::new("refiles");
    let db = open_with_rows(
        &tmp.db_path(),
        28,
        &[
            ("m_import", "mempalace_import", "fact", 0.05),
            ("m_prefixed", "mempalace:rusty_lsp", "fact", 0.05),
        ],
    );
    let conn = db.conn();

    for id in ["m_import", "m_prefixed"] {
        let (memory_type, decay_rate, updated_at) = row(&conn, id);
        assert_eq!(memory_type, "reference", "{id} should have been refiled");
        // decay_rate moves with the type. It is stored per row, so changing
        // memory_type alone would leave these decaying at fact's 0.05 forever
        // and the new type would be cosmetic.
        assert_eq!(decay_rate, REFERENCE_DECAY_RATE, "{id} kept fact's rate");
        // updated_at moves too, so the change reaches other nodes under LWW.
        assert_ne!(
            updated_at, "2026-01-01T00:00:00+00:00",
            "{id} must have a fresh updated_at or a later-upgrading node will \
             push its stale 'fact' back over this"
        );
    }
}

#[test]
fn v29_leaves_everything_else_alone() {
    let tmp = TmpDir::new("narrow");
    let db = open_with_rows(
        &tmp.db_path(),
        28,
        &[
            // Right source, wrong type: the user classified it deliberately.
            ("keep_decision", "mempalace_import", "decision", 0.02),
            // Right type, wrong source: an ordinary fact.
            ("keep_fact", "manual", "fact", 0.05),
            // A source that merely mentions the word.
            ("keep_lookalike", "not_mempalace_import", "fact", 0.05),
        ],
    );
    let conn = db.conn();

    for (id, expected_type, expected_rate) in [
        ("keep_decision", "decision", 0.02),
        ("keep_fact", "fact", 0.05),
        ("keep_lookalike", "fact", 0.05),
    ] {
        let (memory_type, decay_rate, updated_at) = row(&conn, id);
        assert_eq!(memory_type, expected_type, "{id} was reclassified");
        assert_eq!(decay_rate, expected_rate, "{id} had its decay rate moved");
        assert_eq!(
            updated_at, "2026-01-01T00:00:00+00:00",
            "{id} was touched and will now sync a no-op change"
        );
    }
}

#[test]
fn v29_does_not_re_run_on_a_database_already_at_29() {
    // The one step in the reconciler that is not idempotent, and the reason it
    // is version-gated. Every other phase converges on re-run by construction;
    // this one is a reclassification, so a user who deliberately moves a row
    // back to `fact` after upgrading must not have it silently refiled on the
    // next open.
    let tmp = TmpDir::new("gated");
    let path = tmp.db_path();
    {
        let db = open_with_rows(&path, 28, &[("m", "mempalace_import", "fact", 0.05)]);
        let conn = db.conn();
        assert_eq!(row(&conn, "m").0, "reference", "precondition: refiled once");
        // The user disagrees and moves it back.
        conn.execute(
            "UPDATE memories SET memory_type = 'fact', decay_rate = 0.05 WHERE id = 'm'",
            [],
        )
        .unwrap();
    }

    let db = Database::open(&path).unwrap();
    let conn = db.conn();
    let (memory_type, decay_rate, _) = row(&conn, "m");
    assert_eq!(
        memory_type, "fact",
        "a deliberate reclassification must survive the next open"
    );
    assert_eq!(decay_rate, 0.05);
}

#[test]
fn v29_is_a_no_op_on_a_vault_with_no_such_imports() {
    let tmp = TmpDir::new("noop");
    let db = open_with_rows(&tmp.db_path(), 28, &[("plain", "manual", "fact", 0.05)]);
    let conn = db.conn();
    assert_eq!(row(&conn, "plain").0, "fact");
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn a_deleted_row_is_not_refiled() {
    // Tombstones carry no user-visible classification, and touching
    // `updated_at` on one would push a pointless change to every other node.
    let tmp = TmpDir::new("deleted");
    let path = tmp.db_path();
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
                 id TEXT PRIMARY KEY,
                 content TEXT NOT NULL,
                 source TEXT NOT NULL DEFAULT 'manual',
                 memory_type TEXT NOT NULL DEFAULT 'unclassified',
                 decay_rate REAL NOT NULL DEFAULT 0.05,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 deleted_at TEXT
             );
             INSERT INTO memories
                 (id, content, source, memory_type, decay_rate, created_at, updated_at, deleted_at)
             VALUES ('gone', 'body', 'mempalace_import', 'fact', 0.05,
                     '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00',
                     '2026-02-01T00:00:00+00:00');
             PRAGMA user_version = 28;",
        )
        .unwrap();
    }

    let db = Database::open(&path).unwrap();
    let conn = db.conn();
    let (memory_type, _, updated_at) = row(&conn, "gone");
    assert_eq!(memory_type, "fact", "a tombstone must not be refiled");
    assert_eq!(updated_at, "2026-01-01T00:00:00+00:00");
}

#[test]
fn the_schema_version_matches_the_reference() {
    // The number is not this crate's to choose: `remind_me` reads it on open
    // and skips migrating anything already at its own target, so claiming a
    // version the data does not match is what makes a database silently
    // mis-migrated by the other side.
    assert_eq!(SCHEMA_VERSION, 29);
}
