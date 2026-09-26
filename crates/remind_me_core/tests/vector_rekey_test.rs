//! Schema v30 moved vector chunks off `memories.rowid` onto memory ids
//! (ADR-0023 §4). These tests put a database back into the v29 vector
//! layout and check that opening it carries every chunk across, drops what
//! only a reused rowid could have claimed, and leaves nothing of the old
//! tables behind. They also check that a database stamped by a newer build
//! is refused rather than reshaped.

use remind_me_core::db::vectors::Vectors;
use remind_me_core::Database;
use rusqlite::{params, Connection};

struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rrm_rekey_{}_{}_{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDb(dir)
    }

    fn path(&self) -> std::path::PathBuf {
        self.0.join("memory.db")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const NOW: &str = "2026-09-26T00:00:00+00:00";

fn add_memory(conn: &Connection, id: &str) -> i64 {
    conn.execute(
        "INSERT INTO memories (id, content, created_at, updated_at) VALUES (?, 'x', ?, ?)",
        params![id, NOW, NOW],
    )
    .unwrap();
    conn.query_row("SELECT rowid FROM memories WHERE id = ?", [id], |r| {
        r.get(0)
    })
    .unwrap()
}

/// Replace the v30 vector table with the v29 pair, and stamp v29.
fn downgrade_vectors_to_v29(conn: &Connection) {
    conn.execute_batch(
        "DROP TABLE vec_chunks;
         CREATE TABLE vec_chunks (
             vec_rowid    INTEGER PRIMARY KEY,
             memory_rowid INTEGER NOT NULL,
             chunk_ix     INTEGER NOT NULL
         );
         CREATE INDEX idx_vec_chunks_memory ON vec_chunks(memory_rowid);
         CREATE TABLE vec_embeddings (
             vec_rowid INTEGER PRIMARY KEY REFERENCES vec_chunks(vec_rowid),
             embedding BLOB NOT NULL
         );
         PRAGMA user_version = 29;",
    )
    .unwrap();
}

fn put_v29_chunk(conn: &Connection, memory_rowid: i64, chunk_ix: i64, embedding: &[u8]) {
    conn.execute(
        "INSERT INTO vec_chunks (memory_rowid, chunk_ix) VALUES (?, ?)",
        params![memory_rowid, chunk_ix],
    )
    .unwrap();
    let vec_rowid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO vec_embeddings (vec_rowid, embedding) VALUES (?, ?)",
        params![vec_rowid, embedding],
    )
    .unwrap();
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name = ?",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

#[test]
fn opening_a_v29_database_rekeys_every_chunk_onto_its_memory_id() {
    let tmp = TempDb::new("carry");
    {
        let db = Database::open(tmp.path()).unwrap();
        let conn = db.conn();
        let a = add_memory(&conn, "mem_a");
        let b = add_memory(&conn, "mem_b");
        downgrade_vectors_to_v29(&conn);
        put_v29_chunk(&conn, a, 0, &[1, 0, 0, 0]);
        put_v29_chunk(&conn, a, 1, &[2, 0, 0, 0]);
        put_v29_chunk(&conn, b, 0, &[3, 0, 0, 0]);
        // A chunk left behind by a memory that no longer exists: under v29 the
        // next memory to reuse rowid 999 would have inherited it.
        put_v29_chunk(&conn, 999, 0, &[4, 0, 0, 0]);
    }

    let db = Database::open(tmp.path()).unwrap();
    let conn = db.conn();
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, remind_me_core::db::schema::SCHEMA_VERSION);
    assert!(!table_exists(&conn, "vec_embeddings"));
    assert!(!table_exists(&conn, "vec_chunks_v29"));
    assert!(!table_exists(&conn, "idx_vec_chunks_memory"));

    let mut chunks: Vec<(String, Vec<u8>)> = Vectors::new(&conn)
        .all()
        .unwrap()
        .into_iter()
        .map(|c| (c.memory_id, c.embedding))
        .collect();
    chunks.sort();
    assert_eq!(
        chunks,
        vec![
            ("mem_a".to_string(), vec![1, 0, 0, 0]),
            ("mem_a".to_string(), vec![2, 0, 0, 0]),
            ("mem_b".to_string(), vec![3, 0, 0, 0]),
        ]
    );
    assert_eq!(Vectors::new(&conn).chunk_count("mem_a").unwrap(), 2);
}

#[test]
fn the_rekey_runs_once_and_later_opens_leave_the_chunks_alone() {
    let tmp = TempDb::new("idempotent");
    {
        let db = Database::open(tmp.path()).unwrap();
        let conn = db.conn();
        let a = add_memory(&conn, "mem_a");
        downgrade_vectors_to_v29(&conn);
        put_v29_chunk(&conn, a, 0, &[1, 0, 0, 0]);
    }
    drop(Database::open(tmp.path()).unwrap());

    let db = Database::open(tmp.path()).unwrap();
    assert_eq!(Vectors::new(&db.conn()).count().unwrap(), 1);
}

#[test]
fn a_database_from_a_newer_build_is_refused_and_left_untouched() {
    let tmp = TempDb::new("newer");
    {
        let db = Database::open(tmp.path()).unwrap();
        let conn = db.conn();
        add_memory(&conn, "mem_a");
        conn.execute_batch("PRAGMA user_version = 9999;").unwrap();
    }

    let err = Database::open(tmp.path())
        .err()
        .expect("a newer database must be refused");
    assert!(err.to_string().contains("9999"), "{err}");

    let conn = Connection::open(tmp.path()).unwrap();
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 9999);
}
