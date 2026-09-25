//! ANN index over chunk embeddings (gap E8 part 1, issue #155).
//!
//! The fallback paths are tested unconditionally, because they are what runs
//! in every build that does not enable the feature — which is most of them.
//! The equivalence test is the one that matters when it is enabled: an index
//! that returns *different* answers from brute force is worse than no index,
//! because the results still look plausible.

use remind_me_core::ann_index;
use remind_me_core::Database;

#[test]
fn availability_matches_the_compiled_feature() {
    assert_eq!(ann_index::available(), cfg!(feature = "ann"));
}

#[test]
fn an_in_memory_database_has_nowhere_to_persist_an_index() {
    let db = Database::open_in_memory().unwrap();
    // Not an error — just no index. The search path treats it as "fall back".
    assert!(ann_index::index_path(&db.conn()).is_none());
}

#[test]
fn a_missing_index_yields_no_candidates_rather_than_an_error() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    // `None` is the signal to scan everything. A search must never fail
    // because an optimisation was unavailable.
    assert!(ann_index::candidates(&conn, &[0.1, 0.2, 0.3], 10).is_none());
}

#[test]
fn the_live_signature_reports_an_empty_store_honestly() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    remind_me_core::vectors::ensure_schema(&conn).unwrap();

    let (count, dimension) = ann_index::live_signature(&conn).unwrap();

    // Zero vectors at zero dimension is what "nothing to index" looks like,
    // and it is what makes `build` refuse rather than write an empty index
    // that would then read as valid.
    assert_eq!((count, dimension), (0, 0));
}

#[cfg(not(feature = "ann"))]
#[test]
fn building_reports_the_feature_is_missing() {
    let db = Database::open_in_memory().unwrap();
    let err = ann_index::build(&db.conn()).unwrap_err();

    // Names the flag, like the pdf and cloud-backup features do.
    assert!(err.contains("--features ann"), "{err}");
}

#[cfg(feature = "ann")]
mod with_the_feature {
    use super::*;
    use rusqlite::params;

    struct TempDb(std::path::PathBuf);
    impl TempDb {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rrm_ann_{}_{}_{}",
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
            self.0.join("memories.db")
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Store one embedding against a memory, bypassing the embedder.
    fn embed(conn: &rusqlite::Connection, content: &str, vector: &[f32]) -> i64 {
        let id = remind_me_core::db::queries::add_memory(
            conn,
            remind_me_core::MemoryAddInput {
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
        .unwrap()
        .id;
        let rowid: i64 = conn
            .query_row(
                "SELECT rowid FROM memories WHERE id = ?",
                params![&id],
                |r| r.get(0),
            )
            .unwrap();

        // `vec_chunks` first: `vec_embeddings.vec_rowid` is a foreign key into
        // it, so the other order violates the constraint.
        conn.execute(
            "INSERT INTO vec_chunks (memory_rowid, chunk_ix) VALUES (?, 0)",
            params![rowid],
        )
        .unwrap();
        let vec_rowid = conn.last_insert_rowid();

        let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
        conn.execute(
            "INSERT INTO vec_embeddings (vec_rowid, embedding) VALUES (?, ?)",
            params![vec_rowid, bytes],
        )
        .unwrap();
        rowid
    }

    #[test]
    fn an_index_is_built_and_then_narrows_to_real_candidates() {
        let dir = TempDb::new("build");
        let db = Database::open(dir.path()).unwrap();
        let conn = db.conn();
        remind_me_core::vectors::ensure_schema(&conn).unwrap();

        let a = embed(&conn, "alpha", &[1.0, 0.0, 0.0]);
        let b = embed(&conn, "beta", &[0.0, 1.0, 0.0]);
        embed(&conn, "gamma", &[0.0, 0.0, 1.0]);

        assert_eq!(ann_index::build(&conn).unwrap(), 3);

        // A query pointing at `alpha` must at least propose it.
        let found = ann_index::candidates(&conn, &[1.0, 0.0, 0.0], 2)
            .expect("a freshly built index should be usable");
        assert!(found.contains(&a), "got {found:?}");
        assert!(!found.contains(&b) || found.len() > 1);
    }

    #[test]
    fn a_stale_index_is_ignored_rather_than_trusted() {
        let dir = TempDb::new("stale");
        let db = Database::open(dir.path()).unwrap();
        let conn = db.conn();
        remind_me_core::vectors::ensure_schema(&conn).unwrap();
        embed(&conn, "first", &[1.0, 0.0]);
        ann_index::build(&conn).unwrap();

        // The store moves on; the index does not.
        embed(&conn, "second", &[0.0, 1.0]);

        // A stale index quietly returning deleted or missing memories is worse
        // than no index, because the answers still look plausible.
        assert!(
            ann_index::candidates(&conn, &[1.0, 0.0], 2).is_none(),
            "a stale index must not be used"
        );
    }

    #[test]
    fn an_index_built_at_another_dimension_is_ignored() {
        let dir = TempDb::new("dim");
        let db = Database::open(dir.path()).unwrap();
        let conn = db.conn();
        remind_me_core::vectors::ensure_schema(&conn).unwrap();
        embed(&conn, "three-dim", &[1.0, 0.0, 0.0]);
        ann_index::build(&conn).unwrap();

        // A query at a dimension the index was not built for cannot be
        // compared against it at all.
        assert!(ann_index::candidates(&conn, &[1.0, 0.0], 1).is_none());
    }

    #[test]
    fn narrowing_by_the_index_returns_the_same_rows_a_full_scan_would() {
        let dir = TempDb::new("equiv");
        let db = Database::open(dir.path()).unwrap();
        let conn = db.conn();
        remind_me_core::vectors::ensure_schema(&conn).unwrap();

        // A small corpus spread around the unit circle, so the ranking is
        // unambiguous and an approximation that got it wrong would show.
        let mut expected_order = Vec::new();
        for i in 0..12 {
            let angle = i as f32 * std::f32::consts::TAU / 12.0;
            let rowid = embed(&conn, &format!("point {i}"), &[angle.cos(), angle.sin()]);
            expected_order.push((i, rowid));
        }
        ann_index::build(&conn).unwrap();

        let query = [1.0f32, 0.0];

        // What the index proposes.
        let narrowed = ann_index::candidates(&conn, &query, 3)
            .expect("a freshly built index should be usable");

        // What a full scan would rank, computed here rather than through the
        // search path so this test does not depend on an embedder.
        let mut all: Vec<(i64, f32)> = Vec::new();
        let mut stmt = conn
            .prepare(
                "SELECT vc.memory_rowid, ve.embedding
                   FROM vec_chunks vc
                   JOIN vec_embeddings ve ON ve.vec_rowid = vc.vec_rowid",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
            .unwrap();
        for row in rows {
            let (rowid, bytes) = row.unwrap();
            let v: Vec<f32> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c))
                .collect();
            all.push((rowid, query[0] * v[0] + query[1] * v[1]));
        }
        all.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let brute_top3: Vec<i64> = all.iter().take(3).map(|(id, _)| *id).collect();

        // The index over-fetches, so its candidate set is larger — but it must
        // *contain* every row brute force would have ranked in the top 3.
        // Missing one means the ANN path silently returns worse results than
        // the path it replaced, which is the failure worth preventing.
        for rowid in &brute_top3 {
            assert!(
                narrowed.contains(rowid),
                "index missed rowid {rowid} that brute force ranked top-3; \
                 narrowed={narrowed:?} brute={brute_top3:?}"
            );
        }
    }

    #[test]
    fn building_with_nothing_to_index_refuses_rather_than_writing_an_empty_index() {
        let dir = TempDb::new("empty");
        let db = Database::open(dir.path()).unwrap();
        let conn = db.conn();
        remind_me_core::vectors::ensure_schema(&conn).unwrap();

        // An empty index on disk would read as valid and return nothing
        // forever, which is the quietest possible failure.
        let err = ann_index::build(&conn).unwrap_err();
        assert!(err.contains("no embeddings"), "{err}");
    }
}
