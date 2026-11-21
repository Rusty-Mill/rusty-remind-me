//! The R3 store benchmark (#1618) on the backends in this crate. The
//! workloads are in `harness.rs`; candidates outside the workspace (Attest,
//! D45) include that file and add themselves.

use remind_me_core::db::engine::EngineTables;
use remind_me_core::db::sqlite::SqliteTables;

mod harness;

use harness::{Candidate, Result};

fn main() -> Result<()> {
    harness::run(&[
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
