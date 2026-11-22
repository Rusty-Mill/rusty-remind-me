//! The R3 store benchmark (#1618) on the backends in this crate. The
//! workloads are in `harness.rs`; candidates outside the workspace (Attest,
//! D45) include that file and add themselves.

use remind_me_core::db::backend::Backend;
use remind_me_core::db::engine::EngineTables;
use remind_me_core::db::sqlite::SqliteTables;
use std::path::Path;

#[path = "harness.rs"]
mod harness;

use harness::{backup_package, Candidate, Result};

fn engine(dir: &Path) -> Result<Box<dyn Backend>> {
    Ok(Box::new(EngineTables::open(dir)?))
}

fn sqlite(dir: &Path) -> Result<Box<dyn Backend>> {
    Ok(Box::new(SqliteTables::open(dir)?))
}

const ENGINE: Candidate = Candidate {
    name: "engine",
    tables: engine,
    restore: engine,
    package: backup_package,
};

const SQLITE: Candidate = Candidate {
    name: "sqlite",
    tables: sqlite,
    restore: sqlite,
    package: backup_package,
};

fn main() -> Result<()> {
    harness::run(&[ENGINE, SQLITE])
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness::{measure, Dataset, Isolation, Mode};

    /// The smallest store has no editable rows (its one memory is a
    /// dialog): updates and deletes report zero samples and say why, and
    /// the run still completes.
    #[test]
    fn the_minimum_size_reports_skipped_edits() {
        let root = std::env::temp_dir().join(format!("store_bench_min_{}", std::process::id()));
        let report = measure(
            SQLITE,
            Mode::Bench,
            &Dataset::Synthetic(1),
            &root,
            Isolation::InProcess,
        )
        .unwrap();
        for metric in ["update", "delete"] {
            assert_eq!(report[metric]["count"], 0, "{metric}");
            assert!(report[metric]["skipped"].is_string(), "{metric}");
        }
        assert_eq!(report["restore"]["matched"], true);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_zero_size_is_refused_before_any_directory_is_made() {
        let root = std::env::temp_dir().join(format!("store_bench_zero_{}", std::process::id()));
        assert!(measure(
            ENGINE,
            Mode::Bench,
            &Dataset::Synthetic(0),
            &root,
            Isolation::InProcess
        )
        .is_err());
        assert!(!root.exists());
    }

    /// A small engine store backed up as `manual-*.engine`: D1's source.
    fn small_backup(root: &Path) -> std::path::PathBuf {
        use remind_me_core::db::derived::Origin;
        use remind_me_core::db::entities::{Entities, RelationRow};
        use remind_me_core::db::queries;
        use remind_me_core::db::references::{NewReference, References};
        use remind_me_core::db::vectors::Vectors;
        use remind_me_core::entity;
        use remind_me_core::models::{EntityInput, MemoryAddInput};
        use remind_me_core::Database;

        let dir = root.join("source");
        std::fs::create_dir_all(&dir).unwrap();
        let tables = EngineTables::open(&dir.join("store")).unwrap();
        let db = Database::open_backend(Box::new(tables), Some(dir.join("memory.db"))).unwrap();
        let store = db.store();
        let mut ids = Vec::new();
        for i in 0..30 {
            let input = MemoryAddInput {
                content: format!(
                    "memory number {i} about rust storage engines and {}",
                    "words ".repeat(i)
                ),
                category: if i % 3 == 0 { "dialog" } else { "fact" }.into(),
                ..Default::default()
            };
            let id = queries::add_memory(&store, input).unwrap().id;
            let v: Vec<u8> = (0..4 * 8).map(|b| (b + i) as u8).collect();
            Vectors::new(&store).put(&id, 0, &v).unwrap();
            ids.push(id);
        }
        let upsert = |name: &str| {
            let input = EntityInput {
                name: name.into(),
                kind: None,
                aliases: vec![],
            };
            entity::upsert_entity(&store, &input).unwrap().id
        };
        let (a, b) = (upsert("a"), upsert("b"));
        Entities::new(&store)
            .link(&ids[1], &a, "2026-11-22", Origin::Sync)
            .unwrap();
        let relation = RelationRow {
            id: "r1",
            subject_entity_id: &a,
            relation: "knows",
            object_entity_id: &b,
            created_at: "2026-11-22",
            updated_at: "2026-11-22",
            node_id: Some("node-x"),
        };
        Entities::new(&store)
            .insert_relation_or_ignore(&relation, Origin::Sync)
            .unwrap();
        References::new(&store)
            .insert(&NewReference::new(
                &ids[2],
                "url",
                "https://example.com",
                "2026-11-22",
            ))
            .unwrap();
        remind_me_core::backup::create_backup(&store, "manual")
            .unwrap()
            .path
            .into()
    }

    /// D1 loads every row kind through the ports into each candidate, and
    /// the run completes with an isolated restore and a cold open.
    #[test]
    fn d1_loads_the_backup_into_each_candidate() {
        let root = std::env::temp_dir().join(format!("store_bench_d1_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let backup = small_backup(&root);
        for candidate in [ENGINE, SQLITE] {
            let dataset = Dataset::D1(backup.clone());
            let report = measure(
                candidate,
                Mode::Bench,
                &dataset,
                &root,
                Isolation::InProcess,
            )
            .unwrap();
            let loaded = &report["loaded"];
            for (kind, n) in [
                ("memories", 30),
                ("vectors", 30),
                ("entities", 2),
                ("links", 1),
                ("relations", 1),
                ("references", 1),
            ] {
                assert_eq!(loaded[kind], n, "{} {kind}", candidate.name);
            }
            assert_eq!(report["restore"]["matched"], true, "{}", candidate.name);
            assert!(report["cold_open"]["child"]["first_query_ms"].is_number());
        }
        assert!(backup.is_dir(), "the backup itself is left alone");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// D2 grows the loaded store to its total.
    #[test]
    fn d2_grows_d1_to_its_total() {
        let root = std::env::temp_dir().join(format!("store_bench_d2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let backup = small_backup(&root);
        let dataset = Dataset::D2 { backup, total: 50 };
        let report = measure(ENGINE, Mode::Bench, &dataset, &root, Isolation::InProcess).unwrap();
        assert_eq!(report["grown"]["memories"], 20);
        assert_eq!(report["memories"], 50);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_missing_backup_is_refused_before_any_directory_is_made() {
        let root = std::env::temp_dir().join(format!("store_bench_nob_{}", std::process::id()));
        let dataset = Dataset::D1(root.join("no-such-backup"));
        assert!(measure(ENGINE, Mode::Bench, &dataset, &root, Isolation::InProcess).is_err());
        assert!(!root.exists());
    }
}
