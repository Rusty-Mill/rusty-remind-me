//! The R3 store benchmark (#1618) with Attest beside the in-workspace
//! backends: the same workloads, included from remind_me_core's example.
//!
//! ```text
//! cargo run --release --example store_bench -- bench attest 15000 /tmp/bench
//! ```
//!
//! Attest's engine tables go in `<store>.temp`, where the harness counts
//! them (D53). Its G8 package holds the log only, and a restore rebuilds
//! the tables from it (D52).

use remind_me_core::db::engine::EngineTables;
use remind_me_core::db::sqlite::SqliteTables;

#[path = "../../../crates/remind_me_core/examples/store_bench/harness.rs"]
mod harness;

use harness::{backup_package, Candidate, Result};
use std::path::Path;

/// The Attest candidate's tables over the log in `dir`, with the engine
/// tables beside it in `<dir>.temp`.
fn attest(dir: &Path) -> Result<Box<dyn remind_me_core::db::backend::Backend>> {
    Ok(Box::new(attest_store::open_at(
        dir,
        &dir.with_extension("temp"),
    )?))
}

/// Attest's check on its closed log in `dir` (D56): the log's own verify.
/// The candidate's next open then rebuilds the tables from it.
fn verify_attest(dir: &Path) -> Result<serde_json::Value> {
    use attest::store::file::{FileStore, StoreConfig};
    use attest::Log;
    let records = FileStore::open(dir, StoreConfig::default())?.verify()?;
    Ok(serde_json::json!({ "check": "attest log verify", "records": records }))
}

fn main() -> Result<()> {
    harness::run(&[
        Candidate {
            name: "attest",
            tables: attest,
            restore: attest,
            package: |_, store, dest| Ok(attest_store::package(store, dest)?),
            verify: verify_attest,
        },
        Candidate {
            name: "engine",
            tables: |dir| Ok(Box::new(EngineTables::open(dir)?)),
            restore: |dir| Ok(Box::new(EngineTables::open(dir)?)),
            package: backup_package,
            verify: harness::verify_engine,
        },
        Candidate {
            name: "sqlite",
            tables: |dir| Ok(Box::new(SqliteTables::open(dir)?)),
            restore: |dir| Ok(Box::new(SqliteTables::open(dir)?)),
            package: backup_package,
            verify: harness::verify_sqlite,
        },
    ])
}
