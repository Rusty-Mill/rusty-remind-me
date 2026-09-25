# ADR-0022: A storage seam in the node before any storage port

Status: Accepted (2026-09-25)
Date: 2026-09-25

## Context

ADR-0021 moved the hub onto the embedded `rusty_multimodal_db` engine. Its
phase 4 is "a storage port in `remind_me_core`, as groundwork for ever moving
the node". It noted that this pays off only if the Python `remind_me` moves
too, since finishing it breaks Tenet 3 (the shared v29 `memory.db`).

A survey of `remind_me_core` and its callers (2026-09-25) sized that work.
Counts are from grep, so they are approximate.

- **The SQL has no seam to sit behind.**
  - 48 core files import `rusqlite`.
  - About 258 functions take `conn: &Connection`.
  - `db::queries` holds about 23 of roughly 310 SQL call sites; the other
    92% are written inline in domain modules (promotion, sync, wiki,
    vectors, entities, and so on).
  - Outside core, every API route is typed `fn(&Connection, …)`. The MCP
    server and the CLI hand `&Connection` to core; only `remind_me_remote`
    never touches it.
- **The hardest behaviour lives in the database, not in Rust.**
  - Change capture for sync is done by triggers. `memories_outbox_ai/au`,
    `entities_outbox_ai/au` and two graph triggers build the outbox payload
    with `json_object`.
  - Echo suppression reads `MAX(sync_outbox.id)` around a write on the same
    connection.
  - `memories_fts` and `wiki_fts` are kept current by triggers.
  - Search ranks with `bm25` and `snippet`.
  - Vectors are keyed on `memories.rowid`.
- **These triggers cannot move while Tenet 3 holds.** The Python
  `remind_me` writes the same file and relies on the same triggers to fill
  the outbox and the FTS tables. Moving change capture into Rust would leave
  Python's writes unsynced and unindexed.
- **There is no storage trait in core today.**
  - The only traits are `Notifier` and `Embedder`.
  - About 757 test call sites build a real in-memory database
    (`Database::open_in_memory`), and none use a fake.
  - The hub's `HubStore` (ADR-0015) is the precedent. It had two real
    backends on day one.

So a trait-based port now would have exactly one implementation. It could
not cover the parts that make moving the node hard, because those are in
SQL and shared with Python. That is the speculative generality this project's
Tenet 2 warns against, applied to interfaces instead of dependencies.

## Decision

**Build a seam, not a port.** Gather each table group's SQL behind a concrete
repository type in `remind_me_core::db`. Extract traits only when a second
backend is real.

1. **Repositories are concrete and narrow**, one per table group (saved
   searches, reminders, sync bookkeeping, history, and so on).
   - Each is a small struct borrowing a `&Connection`, for example
     `SavedSearches<'c>`.
   - Its methods speak domain types and hold all of that group's SQL.
   - Domain modules keep their logic and call the repository instead of
     writing SQL.
   - There is no single `NodeStore` god-trait: callers depend on the
     narrow type they use.
2. **Behaviour-preserving, one table group per change.**
   - The repository wraps the same connection and runs the same SQL, so the
     757 in-memory test setups keep working unchanged.
   - Each change is reviewable on its own and leaves the schema untouched,
     so Tenet 3 holds throughout.
3. **Order: the isolated groups first, the trigger-bound ones last.**
   1. Saved searches.
   2. Reminder deliveries.
   3. Sync bookkeeping (`sync_log`, `sync_flags` and `sync_sends`, but not
      the outbox).
   4. History and feedback.
   5. Stats and analytics. `PRAGMA`-based sizes become one `storage_info()`.
   6. Last, and only once the Python side has a plan: the outbox and echo
      suppression, FTS search, vectors, `Backup`, and imports. Imports stay
      on SQLite for the file they read, but write through the repositories.
4. **Connection ownership stays in `Database`.**
   - Nothing outside `db` opens a connection. The scheduler, watcher and
     promotion threads, which call `Connection::open` themselves today,
     move to `Database::open_secondary_at`.
   - API handlers and the MCP server keep receiving a connection until a
     repository covers everything a route needs.
5. **Traits come with the second backend.**
   - When a table group gains a real second implementation, its concrete
     repository's methods become the trait, with the SQLite type as the
     first implementation. That backend could be the engine, or a fake a
     test genuinely needs.
   - Because callers already use one narrow type, extracting the trait is
     mechanical.

**Stop criterion.** If Python `remind_me` has no plan to move by the time
step 5 is done, stop there. The seam has paid for itself in cohesion. The
remaining groups gain little until the triggers can move.

## Consequences

- SQL for a table group lives in one file, so a schema change touches one
  module rather than every caller that writes that query inline.
- Nothing becomes swappable yet. Moving the node still needs change capture,
  full-text search and rowid-keyed vectors in whatever replaces SQLite, and
  it still breaks Tenet 3.
- Each step touches `remind_me_api`'s and `remind_me_mcp`'s call sites for
  its table group. Route signatures do not change until the end.

## Alternatives declined

- **A `NodeStore` trait now, with SQLite as its only implementation.** It
  has one implementation and a surface of hundreds of methods, and it
  cannot express the trigger-driven behaviour it would need to abstract.
- **Per-group traits now.** They are narrower, but still one implementation
  each. They would also freeze each interface before a second backend has
  shown what it needs.
- **Move change capture out of triggers first.** This is where the real
  difficulty is, but it breaks Python sync the moment it ships.

## Progress

| Step | Table group | Repository | State |
|---|---|---|---|
| 1 | Saved searches | `db::saved_searches::SavedSearches` | Done |
| 2 | Reminders and deliveries | `db::reminders::Reminders` | Done |
| 3 | Sync bookkeeping | `db::sync_state::SyncState` | Done |
| 4 | History and feedback | | |
| 5 | Stats and analytics | | |

**Step 1.** `saved_searches.rs` keeps the rules: update by name, the id
derived from the name, seeding a first poll, and diffing seen matches. All
of its SQL moved to `db::saved_searches`. Its public functions keep their
signatures, so the API and MCP callers did not change. The 12 existing
saved-search tests pass unchanged, and the repository has 3 tests of its
own:
- an update keeps the stored name and `created_at`;
- malformed `filters` JSON reads as empty;
- `mark_seen` keeps the first sighting.

**Step 2.** `db::reminders` holds the four statements behind reminders:
- whether a memory is live;
- setting or clearing `remind_at` (stamping `updated_at`, which the outbox
  trigger picks up);
- the window query that listing, the digest and the scheduler's "due"
  check all share;
- recording a delivery.

The rules stay in `reminders.rs` and `scheduler.rs`: parsing `remind_at`,
refusing one in the past, and recording a delivery only after it was
attempted. The scheduler thread's own `Connection::open` and its
`PRAGMA database_list` are left for the connection-ownership item (decision
4), because routing them through `Database` changes the thread's busy
timeout. The 42 existing reminder, scheduler, reminder-sync and digest
tests pass unchanged, and they already pin the delivery rules: once only,
a reschedule re-arms, and a deleted memory never fires.

**Step 3.** `db::sync_state` holds every statement that touches only
`sync_log`, `sync_flags` or `sync_sends`:
- both pull cursors (keyset and `hub_seq`);
- the push and pull liveness stamps;
- every remote's stamps;
- the repair reset;
- flags;
- recording sends.

The sync code keeps the policy: a missing cursor means the epoch or
`SEQ_UNKNOWN`, a reset returns to `SEQ_UNKNOWN` rather than 0, and stamps
are best-effort. Four statements also read `sync_outbox`, so they stay
with the outbox for its own step:
- fetching a push batch;
- counting pending rows per remote;
- clearing and backfilling the outbox when sync is toggled;
- pruning sends.

## Related

- ADR-0015 (the hub's `HubStore`, the trait precedent with two real backends)
- ADR-0021 (the hub on `rusty_multimodal_db`; phase 4 is this)
- ARCHITECTURE Tenets 2 and 3
