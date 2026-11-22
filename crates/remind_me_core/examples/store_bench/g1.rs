//! The two G1 behaviours the port suites do not cover, checked on every
//! candidate in every run (spec § Candidates, "Conformance must include"):
//! a dialog row cannot be rewritten in place (D13, #1616), and readers see
//! consistent results while the single writer writes. A failed check fails
//! the run.

use remind_me_core::db::{queries, Store};
use remind_me_core::models::{MemoryAddInput, MemoryUpdateInput};
use remind_me_core::testing::{self, Table};
use remind_me_core::Database;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Barrier;

use super::Result;

/// Rows each of the writer's transactions adds.
const BATCH: i64 = 5;
/// Transactions the writer commits.
const TRANSACTIONS: usize = 40;
/// Threads reading while it does.
const READERS: usize = 4;

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
/// starting count plus whole transactions. `store` holds the process lock
/// and only takes the counts before and after; the threads read through
/// the database's secondary source.
pub fn readers_see_whole_transactions(db: &Database, store: &Store<'_>) -> Result<Value> {
    let source = &db.secondary_source().ok_or("an in-memory database")?;
    let base = testing::count(store, Table::Memories)?;
    let start = &Barrier::new(READERS + 1);
    let done = &AtomicBool::new(false);
    let reads = &AtomicUsize::new(0);
    let torn = &AtomicUsize::new(0);
    std::thread::scope(|scope| -> Result<()> {
        let writer = scope.spawn(move || -> std::result::Result<(), String> {
            let store = source.store();
            start.wait();
            let written = (0..TRANSACTIONS).try_for_each(|t| batch(&store, t));
            done.store(true, Ordering::SeqCst);
            written.map_err(|e| e.to_string())
        });
        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                scope.spawn(move || -> std::result::Result<(), String> {
                    let store = source.store();
                    start.wait();
                    while !done.load(Ordering::SeqCst) {
                        let seen =
                            testing::count(&store, Table::Memories).map_err(|e| e.to_string())?;
                        reads.fetch_add(1, Ordering::SeqCst);
                        if (seen - base) % BATCH != 0 {
                            torn.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    Ok(())
                })
            })
            .collect();
        let mut joined: std::result::Result<(), String> = Ok(());
        for reader in readers {
            if let Err(e) = reader.join().map_err(|_| "a reader panicked".to_string())? {
                joined = Err(e);
            }
        }
        writer.join().map_err(|_| "the writer panicked")??;
        Ok(joined?)
    })?;
    let after = testing::count(store, Table::Memories)?;
    let (reads, torn) = (reads.load(Ordering::SeqCst), torn.load(Ordering::SeqCst));
    let expected = base + BATCH * TRANSACTIONS as i64;
    if torn > 0 || after != expected || reads == 0 {
        return Err(format!(
            "G1: readers saw {torn} partial transactions in {reads} reads; \
             {after} rows after, {expected} expected"
        )
        .into());
    }
    Ok(json!({ "transactions": TRANSACTIONS, "batch": BATCH, "reads": reads, "torn": 0 }))
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
