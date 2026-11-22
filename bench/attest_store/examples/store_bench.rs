//! The R3 store benchmark (#1618) with Attest beside the in-workspace
//! backends: the same workloads, included from remind_me_core's example.
//!
//! ```text
//! cargo run --release --example store_bench -- bench attest 15000 /tmp/bench
//! ```
//!
//! A backup of the Attest candidate is a copy of its temporary engine
//! tables, not of its log, so `restore` opens it as engine tables. That
//! checks the copy, not G8: a self-contained Attest package restored in
//! isolation is still to come.

use remind_me_core::db::engine::EngineTables;
use remind_me_core::db::sqlite::SqliteTables;

#[path = "../../../crates/remind_me_core/examples/store_bench/harness.rs"]
mod harness;

use harness::{Candidate, Result};

fn main() -> Result<()> {
    harness::run(&[
        Candidate {
            name: "attest",
            tables: |dir| Ok(Box::new(attest_store::open(dir)?)),
            restore: |dir| Ok(Box::new(EngineTables::open(dir)?)),
        },
        Candidate {
            name: "engine",
            tables: |dir| Ok(Box::new(EngineTables::open(dir)?)),
            restore: |dir| Ok(Box::new(EngineTables::open(dir)?)),
        },
        Candidate {
            name: "sqlite",
            tables: |dir| Ok(Box::new(SqliteTables::open(dir)?)),
            restore: |dir| Ok(Box::new(SqliteTables::open(dir)?)),
        },
    ])
}
