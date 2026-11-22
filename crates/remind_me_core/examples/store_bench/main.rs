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
    use harness::{measure, Isolation, Mode};

    /// The smallest store has no editable rows (its one memory is a
    /// dialog): updates and deletes report zero samples and say why, and
    /// the run still completes.
    #[test]
    fn the_minimum_size_reports_skipped_edits() {
        let root = std::env::temp_dir().join(format!("store_bench_min_{}", std::process::id()));
        let report = measure(SQLITE, Mode::Bench, 1, &root, Isolation::InProcess).unwrap();
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
        assert!(measure(ENGINE, Mode::Bench, 0, &root, Isolation::InProcess).is_err());
        assert!(!root.exists());
    }
}
