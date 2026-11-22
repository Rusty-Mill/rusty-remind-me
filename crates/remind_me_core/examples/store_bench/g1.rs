//! The two G1 behaviours the port suites do not cover, checked on every
//! candidate in every run (spec § Candidates, "Conformance must include"):
//! a dialog row cannot be rewritten in place (D13, #1616), and readers see
//! consistent results while the single writer writes. A failed check fails
//! the run. The second is smoke evidence only, reported with its scope
//! ([`SCOPE`]): it checks row counts, not each transaction or a content
//! snapshot.

use remind_me_core::db::{queries, Store};
use remind_me_core::models::{MemoryAddInput, MemoryUpdateInput};
use remind_me_core::testing::{self, Table};
use remind_me_core::Database;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Barrier;
use std::time::Duration;

use super::Result;

/// What the whole-transaction check shows, so no report reads it as more:
/// every row count a reader saw was a whole number of transactions. It
/// does not show that each transaction was observed, nor that a reader's
/// view of row contents was one consistent snapshot.
const SCOPE: &str = "smoke: row counts are whole transactions; \
                     not per-transaction observation or content snapshots";

/// Rows each of the writer's transactions adds.
const BATCH: i64 = 5;
/// Transactions the writer commits.
const TRANSACTIONS: usize = 40;
/// Threads reading while it does.
const READERS: usize = 4;
/// The writer's pause between transactions, so readers get in between
/// them (the gate makes them wait out each open transaction).
const PAUSE: Duration = Duration::from_millis(2);

/// An update and a delete of the dialog `id` are both refused, and its
/// row reads back unchanged.
pub fn dialog_rewrite_refused(store: &Store<'_>, id: &str) -> Result<Value> {
    let before = row(store, id)?;
    let update: MemoryUpdateInput = serde_json::from_value(json!({
        "memory_id": id,
        "content": "rewritten in place",
    }))?;
    let update_refused = queries::update_memory(store, &update).is_err();
    let delete_refused = queries::delete_memory(store, id).is_err();
    let unchanged = row(store, id)? == before;
    if !(update_refused && delete_refused && unchanged) {
        return Err(format!(
            "G1: dialog {id} was not protected: update refused {update_refused}, \
             delete refused {delete_refused}, row unchanged {unchanged}"
        )
        .into());
    }
    Ok(json!({ "update_refused": true, "delete_refused": true, "unchanged": true }))
}

/// One writer commits [`TRANSACTIONS`] transactions of [`BATCH`] memories
/// while [`READERS`] threads count memories; every count must be the
/// starting count plus whole transactions, and the readers must have seen
/// at least [`TRANSACTIONS`] reads and some count strictly between the
/// first and the last, or the check proved nothing. `store` holds the process lock
/// and only takes the counts before and after; the threads read through
/// the database's secondary source.
pub fn readers_see_whole_transactions(db: &Database, store: &Store<'_>) -> Result<Value> {
    let source = &db.secondary_source().ok_or("an in-memory database")?;
    let base = testing::count(store, Table::Memories)?;
    let start = &Barrier::new(READERS + 1);
    let done = &AtomicBool::new(false);
    let reads = &AtomicUsize::new(0);
    let torn = &AtomicUsize::new(0);
    let between = &AtomicUsize::new(0);
    let mut observed = BTreeSet::new();
    std::thread::scope(|scope| -> Result<()> {
        let writer = scope.spawn(move || -> std::result::Result<(), String> {
            let store = source.store();
            start.wait();
            let written = (0..TRANSACTIONS).try_for_each(|t| {
                std::thread::sleep(PAUSE);
                batch(&store, t)
            });
            done.store(true, Ordering::SeqCst);
            written.map_err(|e| e.to_string())
        });
        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                scope.spawn(move || -> std::result::Result<BTreeSet<i64>, String> {
                    let store = source.store();
                    let mut states = BTreeSet::new();
                    start.wait();
                    while !done.load(Ordering::SeqCst) {
                        let seen =
                            testing::count(&store, Table::Memories).map_err(|e| e.to_string())?;
                        reads.fetch_add(1, Ordering::SeqCst);
                        if (seen - base) % BATCH != 0 {
                            torn.fetch_add(1, Ordering::SeqCst);
                        }
                        states.insert(seen);
                        if seen > base && seen < base + BATCH * TRANSACTIONS as i64 {
                            between.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    Ok(states)
                })
            })
            .collect();
        let mut joined: std::result::Result<(), String> = Ok(());
        for reader in readers {
            match reader.join().map_err(|_| "a reader panicked".to_string())? {
                Ok(seen) => observed.extend(seen),
                Err(e) => joined = Err(e),
            }
        }
        writer.join().map_err(|_| "the writer panicked")??;
        Ok(joined?)
    })?;
    let after = testing::count(store, Table::Memories)?;
    let [reads, torn, between] = [reads, torn, between].map(|c| c.load(Ordering::SeqCst));
    let expected = base + BATCH * TRANSACTIONS as i64;
    if torn > 0 || after != expected || reads < TRANSACTIONS || between == 0 {
        return Err(format!(
            "G1: readers saw {torn} partial transactions in {reads} reads \
             ({between} mid-run); {after} rows after, {expected} expected"
        )
        .into());
    }
    Ok(json!({
        "transactions": TRANSACTIONS,
        "batch": BATCH,
        "reads": reads,
        "mid_run_reads": between,
        "states_observed": observed.len(),
        "torn": 0,
        "scope": SCOPE,
    }))
}

/// The `t`th transaction: [`BATCH`] memories, committed together.
fn batch(store: &Store<'_>, t: usize) -> Result<()> {
    store.transaction(|s| -> Result<()> {
        for i in 0..BATCH {
            let input = MemoryAddInput {
                content: format!("g1 transaction {t} row {i}"),
                category: "fact".into(),
                ..Default::default()
            };
            queries::add_memory(s, input)?;
        }
        Ok(())
    })
}

/// A memory's content, category and metadata.
fn row(store: &Store<'_>, id: &str) -> Result<Vec<Option<Value>>> {
    ["content", "category", "metadata"]
        .iter()
        .map(|c| Ok(testing::memory_column(store, id, c)?))
        .collect()
}
