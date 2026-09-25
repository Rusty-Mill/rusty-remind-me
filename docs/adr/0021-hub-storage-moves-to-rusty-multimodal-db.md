# ADR-0021: The hub's storage moves to an embedded rusty_multimodal_db

Status: Proposed
Date: 2026-09-25

## Context

The owner asked to replace rusty_remind_me's SQLite and Postgres backends with
`rusty_multimodal_db`, the record store built in the same monorepo. Two surveys
and a method-by-method mapping (2026-09-24) sized that request. They split it
in two.

**The node's SQLite stays, for now.** It is not only storage: ARCHITECTURE
Tenet 3 makes `~/.remind-me/memory.db` a file this product shares with the
Python `remind_me`, at an identical v29 schema that CI checks daily against
the reference. The node also leans on SQLite features `rusty_multimodal_db`
does not have: FTS5 search with `bm25`/`snippet`, JSON1, 15 triggers
(including the ones that fill the sync outbox), nullable columns, floats, and
`OR`/`LIKE`/subqueries. About 48 core files call `rusqlite` directly, with no
storage seam to swap behind. Moving the node is a separate project (below).

**The hub can move.** ADR-0015 already put its storage behind the 14-method
`HubStore` trait, chosen at startup. `rusty_multimodal_db` was grown toward
exactly this consumer (its ADR-0045 and ADR-0062; the 2026-09-07 hub spike).

But `rusty_multimodal_db`'s built-in `Memory`, `Entity` and `Relation` types
cannot hold the hub's data:

| Hub needs | Built-in types have |
|---|---|
| `hub_seq`, stamped at write time, pageable | No field and no server-side sequence (its ADR-0055 declined one) |
| `node_id` (author) and `origin_node` (pusher) | One `node_id` |
| 28 memory columns, including `f64` and nullable text | 13 fields; 14 columns have no home |
| Entity `created_at`/`updated_at`/node fields (LWW, alias merge) | None |
| Link `created_at`; a link may arrive before its memory | No edge metadata; the edge is refused |
| µs timestamps | ms |
| Free-form string ids, paged in byte order (`since_id`) | Ids must be `u32`/`i64`/`Uuid`, and ids and sort keys must be `Copy`, which a `String` is not |

Two further constraints:

- **Workspace layers.** An apps crate may not depend on an apps crate from
  another family (`.github/scripts/check_workspace_layers.py`), so the hub
  cannot depend on `rusty_multimodal_db` as it stands.
- **The Postgres backend is the drop-in for the Python hub's own database**
  (ADR-0015). Whatever replaces it must be able to take that database over.

## Decision

**The hub embeds `rusty_multimodal_db`'s generic storage engine in-process,
with record types the hub owns.**

1. **Embedded, not over the wire.** The engine's `GenericProductionStore`
   gives `with_exclusive`: a read, compare, write and stamp under one lock.
   That one primitive covers `apply_record`'s last-writer-wins, the entity
   alias merge, and `hub_seq` stamping, with `hub_seq` visible in assignment
   order. The hub is already the network server, so a second process
   (`memory_server`) would add a hop, a deployment, a connection pool and
   wire-protocol work without buying anything the hub uses.
2. **Hub-owned record types.** The hub defines its own records carrying every
   column it stores today, `hub_seq` and `origin_node` included, and models
   `memory_entities` as a record table so links keep `created_at` and can
   arrive before their memory. Every gap in the table above becomes adapter
   work, and `rusty_multimodal_db`'s protocol and built-in types do not
   change, except ids, which hub-owned types cannot fix (item 3).
3. **String ids map to `Uuid`s, and ids are capped at 64 bytes.** A record's
   engine id is a `Uuid` derived from its string id (v5), and the record keeps
   the original string; a second string mapping to an existing `Uuid` is
   refused, not merged. Keyset paging keeps today's byte order by sorting on a
   fixed-size key: `(updated_at µs, id bytes zero-padded to 64)`. The cost is a
   behaviour change: the hub refuses ids longer than 64 bytes, which it accepts
   today. Real ids are 12 to 36 characters, and the copy tool reports any
   longer ones before a migration.
4. **The engine moves to the libs layer.** The generic store is extracted into
   a new libs crate, which both `rusty_multimodal_db` and the hub depend on.
   `rusty_multimodal_db` re-exports it, so its own paths do not change. First
   the engine must be untangled from the Dog research code: `DurabilityError`
   wraps the Dog store's `StoreError`, and `durability/record_blob.rs` depends
   on `DogRecord`. That cleanup is worth doing on its own account. This is a
   new decision, not a reversal: `rusty_multimodal_db`'s ADR-0043 declined
   splitting out a *client* crate, not the storage engine.
5. **The adapter owns its lock.** `GenericProductionStore::with_exclusive`
   panics on a poisoned lock, so one panic in adapter code would make every
   later request panic until a restart. Today's SQLite hub store recovers
   with `PoisonError::into_inner`. The adapter wraps the raw store stack in
   its own `RwLock` with that same recovery, rather than using
   `GenericProductionStore`.
6. **Postgres and SQLite hub stores are retired, not deleted in the same
   change.** `MultimodalHubStore` becomes a third `HubStore` backend behind a
   feature; the existing hub tests run against all three; a copy tool moves a
   Postgres hub (including the Python hub's legacy schema) or a SQLite hub
   into the new store; the default switches; the old stores go one release
   later. The copy tool's Postgres reader outlives them behind an import
   feature, so a hub that migrates late is not stranded.
7. **The copy preserves `hub_seq` exactly.** Every node stores the last
   `hub_seq` it pulled from this hub and resumes from it
   (`remind_me_core/src/sync/pull.rs`). A copy that renumbered would make every
   node silently skip every row up to its old cursor. The copy carries each
   row's `hub_seq` across unchanged and starts the new counter above the
   highest one; a test pins this.

### Phases

1. Extract the storage engine into a libs crate, with the Dog decoupling
   (a `rusty_multimodal_db` change, with its own ADR superseding the part of
   its ADR-0043 that declined splitting the crate).
2. `MultimodalHubStore` behind a feature, run against the whole hub test
   suite alongside SQLite and Postgres. It starts with a spike on the one
   unproven piece: three sort orders on one store (`hub_seq`, `updated_at`,
   `created_at`). Stacked `Ordered` layers expose only the outer one's
   `PageBy`, and the inner ones only read-only through `.inner()`.
3. Copy tool, default switch, then removal of the old backends.
4. Later and separate: a storage port in `remind_me_core`, as groundwork for
   ever moving the node. It only pays off if the Python `remind_me` moves too,
   since completing it breaks Tenet 3.

## Consequences

- **The whole hub dataset lives in RAM** (the engine keeps records in a
  `HashMap`). That suits a single-user hub; it would not suit a large shared
  one.
- **Lost against running `memory_server`:** the server-only redo journal,
  MVCC and metrics. `with_exclusive` does not roll back, so a crash part-way
  through a multi-record write leaves the earlier records applied.
  `apply_record` writes one record, and `compact_tombstones` is safe to re-run,
  so the hub needs neither.
- **Pushes block pulls.** Every write holds the store's one write lock through
  its `fsync`, and pulls wait on it; the SQLite hub, in WAL mode, let reads run
  alongside writes. Phase 2 measures pull latency under push load before the
  default switches.
- **Disk grows until compaction.** Every insert or replace appends the whole
  record to the store's insert log and `fsync`s it, and only a reopen or an
  explicit `compact()` folds the log in. `compact()` holds the write lock for
  its whole run. The hub schedules compaction and exposes it, as it already
  does for tombstones.
- **Operations change.** Backups are a copy of the data directory taken under
  the hub's lock, not `pg_dump`, and the hub takes the data-directory lock
  (`rusty_multimodal_db` ADR-0092) itself, since there is no separate server
  to take it.
- **Layout upgrades are hand-written.** The engine refuses an old record
  layout and has no in-place upgrade. Every change to a hub record type needs
  a conversion, as `rusty_multimodal_db`'s `migrate_memory_v1_to_v2` does.
- **No SQL on the hub.** Counts and group-bys become code over the store's
  indexes rather than queries. The hub's queries are few and fixed, so this is
  bounded.
- **Removing Postgres drops the "reads the Python hub's database" drop-in**
  that ADR-0015 kept it for. The copy tool replaces it as a one-off step.

## Alternatives declined

- **Talk to a `memory_server` over the network, with only the client moved to
  libs.** The client split is small, but every gap in the table becomes a
  `rusty_multimodal_db` change: Memory@3 and Entity@2 layouts, nullable and
  float fields, a server-stamped sequence, string ids and edge metadata.
  Several of those are large protocol changes. Worth it only if other
  programs had to share the server's data; none does.
- **Move the node off SQLite now.** Breaks interoperability with the Python
  `remind_me` and needs full-text search, JSON functions and triggers in
  `rusty_multimodal_db` first. Deferred to phase 4.
- **An allow-list exception in the layer check.** Quicker than extracting the
  engine, but weakens a rule the whole workspace relies on.

## Phase 2 notes (2026-09-25)

`MultimodalHubStore` (`remind_me_hub/src/store/multimodal/`) is the third
backend, behind the off-by-default `multimodal-store` feature and selected by
`REMIND_ME_HUB_DATA_DIR`. The spike came first
(`rusty_multimodal_db_engine/tests/stacked_ordered.rs`): three stacked
`Ordered` layers page correctly through writes and a reopen, so no engine
change was needed. Building it settled details the decisions above left open:

- **Four engine stores, one per table,** in one data directory. Memories stack
  two sort orders (`hub_seq`; `(updated_at, id)`), the other tables one each.
  Each store's equality index is keyed on the engine id, since the hub looks
  nothing up by value.
- **NUL bytes are refused in ids, as well as ids over 64 bytes.** Zero padding
  cannot tell `"m"` from `"m\0"`, and Postgres TEXT cannot hold NUL either.
  A link's engine id is derived from a length-prefixed pair, so `"a|b"+"c"`
  and `"a"+"b|c"` stay distinct.
- **`hub_seq` has a floor file.** The counter restarts from the highest stored
  `hub_seq`, which a tombstone compaction can delete. Before deleting,
  `compact_tombstones` records the counter, so a reopen never issues a number
  again. The SQLite store's `MAX(hub_seq)+1` has that reuse bug today.
- **Compaction runs hourly** (`REMIND_ME_HUB_COMPACT_INTERVAL_SECS`) when
  anything was written, and on every `/admin/compact_tombstones`.
- **A cursor timestamp is canonicalised** before it becomes microseconds, and
  an unparseable one is a storage error (500) where the SQL stores compare the
  bytes as sent.
- **The route suite runs against SQLite and this store, and a differential
  script against all three.** Postgres's `hub_seq` has gaps (`nextval()` spends
  a number on an LWW loss), so the differential compares it by order there;
  SQLite and this store agree exactly.

Not yet measured: pull latency under push load, which this ADR requires before
the default switches (phase 3).

## Phase 3 notes: the copy tool (2026-09-25)

`rusty-remind-me-hub-copy` (`remind_me_hub/src/bin/copy.rs`, the `import`
module) reads a SQLite or Postgres hub and writes a new engine data directory
in one pass (`MultimodalHubStore::create_from_snapshot`).

- **Rows are read as JSON and parsed like a push.** Each reader turns a row
  into a column-keyed object: SQLite by column, Postgres by `to_jsonb(row)`.
  `record::parse` then validates it and fills defaults, so a legacy schema
  with missing columns reads correctly and is never migrated.
  - Stored empty strings in `category`, `source`, `client`, `status` and
    `memory_type` are kept as they are. A push would replace them with
    defaults.
  - A row that does not parse is reported, not dropped.
- **Where `hub_seq` starts.** A memory with no `hub_seq` gets one in
  `(updated_at, id)` order, as the stores' own `migrate` backfills them. The
  counter starts above the source's high-water mark, which can be above
  every remaining row: for Postgres, the sequence's `last_value`; for
  SQLite, the `hub_meta` mark it has kept since it stopped reissuing a
  compacted `hub_seq`. Tests for both show the next write gets the same
  `hub_seq` on the copy as on the source, including after the source
  compacted away its newest row.
- **The Postgres reader is behind `postgres-import`, not `postgres-store`,**
  so it outlives the Postgres store (decision 6).
- **The copy refuses rather than drops.** Ids the engine cannot hold, and two
  rows mapping to one engine id, stop the copy with nothing written, unless
  `--drop-invalid` says to go ahead without those rows.

### Pull latency under push load (measured 2026-09-25)

`remind_me_hub/examples/pull_latency.rs` preloads a hub on disk with 20 000
memories. Pushers then apply one memory at a time while two pullers page
`since_seq` pulls of 500 from random cursors. It ran on this project's
shared CI-class container.

| 20k preloaded, 10 s | SQLite | Engine, as first built | Engine, fixed |
|---|---|---|---|
| Preload, one writer | 5.7 s | 25.1 s | 5.2 s |
| 4 pushers: pushes/s | 2534 | 259 | 1102 |
| 4 pushers: pull p50 / p99 | 27 / 232 ms | 10 s / 10 s | 3.2 / 7.1 ms |
| 1 pusher: pushes/s | 828 | 308 | 1893 |
| 1 pusher: pull p50 / p99 | 6.5 / 38 ms | 4.2 / 12 ms | 3.6 / 8.9 ms |

The engine as first built failed the Consequences' "pushes block pulls"
concern badly. Three fixes brought it well past SQLite on pulls:

- **Pulls starved behind queued pushes.** `std`'s `RwLock` lets a waiting
  writer go ahead of waiting readers, so under several pushers a pull
  waited for every queued push. Writers now queue on a mutex first, so at
  most one writer waits on the lock.
- **Every insert read the whole insert log** (an engine bug: four header
  bytes read as the whole file). It was fixed in the engine, which made
  writes linear again.
- **Pulls held the read lock while building JSON.** They now copy their
  rows under the lock and serialise after releasing it.

Contended pushes remained below SQLite's, because each engine write
synced its insert log (the slot file is never synced per write) and SQLite
amortises its commits better under contention. A node's first full sync of
20 000 memories spent about 20 s in the store.

### Group commit

The engine now has a batch API, `GroupCommit`:
- `defer_sync()` stops each write syncing its insert-log entry;
- `commit()` syncs them all at once.

The hub gets a matching `HubStore::apply_records`, which `/sync/push` calls
once per request:
- The SQL stores keep the default, which applies records one by one.
- The engine store applies a push in chunks of 64 records. Each chunk takes
  one hold of the write lock and one sync per table touched, and pulls get
  in between chunks.
- Each record is still isolated. A refused id or an LWW loss affects only
  that record's outcome.
- A failed sync fails every record in its chunk. It also stops the store
  taking writes until a restart, and `/health` fails, since the chunk is
  applied in memory but may not be on disk.

Same benchmark, 20k preloaded, 10 s, with pushers sending batches of 100
records (a node's outbox sends up to 200):

| 20k preloaded, 10 s | SQLite | Engine, record by record | Engine, group commit |
|---|---|---|---|
| 4 pushers: pushes/s | 2780 | 1327 | 17 040 |
| 4 pushers: pull p50 / p99 | 46 / 399 ms | 3.0 / 7.1 ms | 4.2 / 11 ms |
| 1 pusher: pushes/s | 1920 | 2148 | 17 800 |
| 1 pusher: pull p50 / p99 | 30 / 61 ms | 3.3 / 7.6 ms | 3.9 / 10 ms |

The record-by-record column is the same run with single-record pushes;
SQLite's single-record figures (2485/s with 4 pushers, 912/s with one) are
close to those above.

Group-commit runs reach 115 000 memories within the 10 s. At that size,
one pull in each run waited about 0.2 to 1 s. Timing the lock hold placed the
wait in a single insert at row 114 729, just past 7/8 of 2^17: the engine
core's `HashMap`s regrowing. The sync took under 1 ms.
The pause happened once each time the row count doubled, batched or not.
It was all under the write lock: the record map held each ~800-byte
memory inline, so a regrow copied every row into a new table of twice the
size. The core now boxes its records, so a regrow moves a key and a pointer
per row:

| Worst lock hold at a regrow | Records inline | Records boxed |
|---|---|---|
| 115 000 rows | 170–370 ms | under 10 ms |
| 229 000 rows | not measured | 37 ms |

In the same 20 s run, the worst pull was 42 ms, and p99 stayed at 10 ms.
A regrow is still linear in the row count (an estimated 0.15 s
at a million rows). A map that grows incrementally would remove the pause
entirely, but it would add a dependency to the engine for sizes a personal
hub does not reach.

### The default switch

The engine is now the default store for a new hub:

- `remind_me_hub`'s default features gain `postgres-import`, which brings in
  the engine and the copy tool. `postgres-store` stays in the defaults.
- `setup.sh install` installs the engine unless given `--postgres` or
  `--sqlite`.
- `deploy/docker-compose.engine.yml` is the documented default for Compose.
- The image and the release archives ship `rusty-remind-me-hub-copy`.

Existing deployments are left alone on purpose:

- `setup.sh` takes the backend from an existing `hub.env`, and refuses a flag
  that contradicts it. Re-running `install` on a Postgres hub would otherwise
  swap its units for the single-container one and strand its database.
- The existing `docker-compose.yml` stays on Postgres, since a changed default
  there would bring up an empty hub on the next `docker compose up`.
- Fly and Railway stay on managed Postgres.

Building the default found that every container path had been broken since
the monorepo import. The Containerfile, `setup.sh` and every template built
from `crates/apps/rusty_remind_me`, which holds no workspace manifest. Two
fixes:

- They now build from the monorepo root, with BuildKit cache mounts in place
  of the standalone repo's stub-manifest trick.
- The image now creates `/data` owned by the hub user. Otherwise a Compose
  named volume came up root-owned and the hub could not create its store. The
  existing SQLite Compose file had the same latent problem.

The image was built and run end to end:
- the engine on a fresh named volume;
- the health check;
- a restart;
- the copy tool moving a SQLite hub onto the engine.

Still to do in phase 3: remove the old stores a release later.

## Related

- The same mapping found that the Postgres hub assigned `hub_seq` at statement
  time, so concurrent pushes could commit out of order and a node on the
  `since_seq` cursor could skip rows for good. That is fixed independently of
  this migration (transaction-scoped advisory lock, v0.2.3), because the
  Postgres backend stays in use until phase 3.
