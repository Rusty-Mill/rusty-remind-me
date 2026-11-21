# ADR-0035: The storage port is one trait per table group

Status: Accepted (2026-11-16, confirmed by Nano); every table group has a port (saved searches, reminders, history, feedback, stats, snapshots, the memories core, archives, entities, curation, promotions, sync state, outbox, imports, references, related, sessions, vectors, wiki); step 6 done (2026-11-21): the store holds a `db::backend::Backend`
Date: 2026-11-16

Continues ADR-0022, whose "extract traits only when a second backend is
real" condition this ADR records as met.

## Context

Millwright R2 asks for "the mirrored storage layers [to be] one interface",
so that R3 can measure the current engine, SQLite and Ledger's store on the
same tests (rusty_mill `docs/projects/millwright/ROADMAP.md`). Nano chose
that order on 2026-11-15: refactor first, then pick the store at the
benchmark (rusty_mill DECISIONS.md `remind-me-order`; card #762; the audit
in `docs/design/dbs-modularity-audit.md`, seam 6).

The mirror is two layers that say the same thing twice:

- `db::<group>` holds a repository (`SavedSearches<'c>`, `Memories<'c>`,
  and 16 more). Most of its methods lock the engine and call
- `db::engine::<group>`, free functions over `EngineTables` with the same
  names and nearly the same signatures.

The interface exists only by convention. Nothing a second backend could
implement, and no test that it could be held to.

ADR-0022 deferred traits until a second backend was real, because a trait
with one implementation is speculative. The R3 benchmark is that second
backend: it is committed work (Millwright R2 to R3), and it cannot run
without an interface to run against.

## Decision

1. **One port trait per table group, declared beside its repository.** For
   example `db::saved_searches::SavedSearchStore`. Its methods speak domain
   types, return `db::Result`, and take `&self` to read and `&mut self` to
   write. There is still no single `NodeStore` god-trait (ADR-0022, item 1).
2. **The engine is an adapter.** `impl SavedSearchStore for EngineTables`
   in `db::engine::saved_searches` replaces that group's free functions, so
   the interface is written once. Reads that cannot fail on the engine
   still return `Result`, because a SQLite or Ledger adapter can fail.
3. **Callers do not change.** The repository stays what domain code takes.
   It locks the tables once per call and goes through the port, as it locked
   them before. Holding a guard across calls was rejected: the lock is not
   reentrant, so a caller that reached a second repository while holding
   one would deadlock.
4. **Behaviour is written once, as a conformance suite.** Each port has a
   `#[cfg(test)] conformance` module of checks over `&mut dyn <Port>` and a
   macro that turns them into one `#[test]` each for a given constructor.
   The engine adapter runs it on temporary tables; a SQLite adapter in R3
   invokes the same macro. Engine-specific tests (reopen, id hashing) stay
   in the adapter's own module. A group whose rows hang off another group
   that has no port yet (reminders hang off memories) takes that backend's
   own fixtures as macro arguments, such as adding and deleting a memory;
   they go once that group has a port.
5. **One table group per commit,** a few per pull request, in ADR-0022's order: the isolated
   groups first (saved searches, reminders, history, feedback, stats), the
   memories core last. Each step is behaviour-preserving and leaves the
   schema and the on-disk format untouched.
6. **Last step: the store holds a backend, not the engine.** Once every
   group has a port, `Store` holds the tables behind a supertrait of all of
   them, and the transaction (`Store::transaction`, today the engine's page)
   becomes part of that backend. That is the point where R3 can open a
   `Database` on another store and run the whole test suite on it.

## The golden set

R2's other half, "a 50-query golden set with expected results runs in CI",
already exists: `crates/remind_me_core/tests/retrieval_eval_test.rs` scores
82 queries against 108 synthetic memories with recall@5, recall@10 and MRR,
gates on `baseline.json`, and runs in every CI test job. It opens its store
with `Database::open_in_memory`, so after step 6 it runs unchanged on any
backend. R3 measures on real data; this set stays the regression gate.

## Step 6, as built (2026-11-21)

- **`db::backend::Backend`** is the supertrait of all nineteen ports, plus
  `Send + Any` and six whole-store operations: `begin_transaction`,
  `commit_transaction`, `rollback_transaction`, `rebuild_indexes`,
  `compact` and `backup_to`. The engine implements it (`EngineTables`;
  a transaction is one page).
- **The store holds a backend.** `BackendLock` (the old `EngineLock`, with
  its transaction gate) holds a `Box<dyn Backend>`; its guard derefs to
  `dyn Backend`, so each repository calls its port on it unchanged.
  `Store::engine()` and `Store::core()` became one `Store::backend()`.
- **The transaction is the backend's.** `Store::transaction` opens a
  `backend::Transaction`, which calls the three transaction methods and
  rolls back when dropped uncommitted. The engine's page stays the
  engine's.
- **Transactions do not nest, and an unsettled transaction fails the
  store closed.** A begin on a thread that already holds a transaction is
  refused before it reaches the backend and leaves the outer transaction's
  gate alone (before step 6 the engine's gate let a nested begin release
  the outer ownership). `Backend::commit_transaction` names the four
  states a failed commit may leave, and `Backend::transaction_state`
  reports `Closed`, `Open` or `NeedsRecovery`. After a commit or a
  rollback the wrapper rolls back anything still open and opens the gate
  only on `Closed`; anything else fails the `BackendLock` closed, so
  `lock()` (now fallible) refuses every read and write on every thread
  until the database is reopened. On the engine a redo-journal error is
  indeterminate (the journal can fail after a whole frame, when its sync
  fails), so the page and the undo log are left in place for reopening
  to settle; only a batch that never reached the journal is put back at
  once.
- **`Database::open_backend(Box<dyn Backend>, path)`** opens a database on
  any backend and runs the same start-up work as `Database::open`. This is
  what R3 calls to run the suite on SQLite or Attest.
- **The four reaches.** `derived::rebuild_indexes`, `backup` and
  `Database::compact_store` go through the backend. The vitality rule is
  `KeywordFilter::vital_enough`, pure logic beside the filter. Only
  `crate::testing` (and engine-only tests) still reach engine rows: they
  find the engine through `Any` (`engine::testing::tables`) and refuse
  with `StoreError::Invalid` on any other backend. Opening and upgrading
  an old `memory.db` stays engine-only.

**What a second backend still needs before the whole suite runs on it:**
an adapter for every port, and a choice for the tests that plant raw rows
through `crate::testing` (they will refuse, so R3 either gives those
helpers a port or reports those tests as engine-only). The integration
tests open `Database::open_in_memory`, which is the engine; R3's harness
picks the backend.

## Not decided here

- Which stores go in the benchmark (Millwright QUESTIONS.md R2) and the
  SQLite adapter itself: R3.
- Whether the repository facade can go once a backend handle exists. It
  holds the per-call locking, so it stays until step 6 shows otherwise.

## Consequences

- No schema, data or API change. The MCP, API and CLI crates are untouched.
- A group's port can be implemented on another store before the rest are,
  and checked against the conformance suite in isolation.
- Since step 6, any `Backend` can back a running `Database`
  (`Database::open_backend`); production still opens the engine.
