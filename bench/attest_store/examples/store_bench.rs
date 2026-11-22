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
    Ok(Box::new(attest_store::open_at(dir, &dir.with_extension("temp"))?))
}

fn main() -> Result<()> {
    harness::run(&[
        Candidate {
            name: "attest",
            tables: attest,
            restore: attest,
            package: |_, store, dest| Ok(attest_store::package(store, dest)?),
        },
        Candidate {
            name: "engine",
            tables: |dir| Ok(Box::new(EngineTables::open(dir)?)),
            restore: |dir| Ok(Box::new(EngineTables::open(dir)?)),
            package: backup_package,
        },
        Candidate {
            name: "sqlite",
            tables: |dir| Ok(Box::new(SqliteTables::open(dir)?)),
            restore: |dir| Ok(Box::new(SqliteTables::open(dir)?)),
            package: backup_package,
        },
    ])
}
