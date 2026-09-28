# ADR-0023: The node's storage moves to rusty_multimodal_db

Status: Proposed
Date: 2026-09-26

## Context

ADR-0021 moved the hub onto the embedded `rusty_multimodal_db` engine and
left the node on SQLite. It gave two reasons:
- **A shared file.** The Python `remind_me` shares `~/.remind-me/memory.db`
  with the node, at the identical v29 schema (ARCHITECTURE Tenet 3).
- **SQLite features.** The node leans on things the engine lacks.

ADR-0022 then put a seam around five table groups and parked the rest
until the Python side had a plan.

The owner has now decided three things (2026-09-26):
1. **The node moves off SQLite.** `rusty_multimodal_db` gains whatever the
   node needs, so SQLite goes away.
2. **The Python `remind_me` is retired first.** Nothing needs to share
   `memory.db`, so Tenet 3 ends. Interop with other nodes stays at the sync
   protocol, which is unchanged.
3. **Two design calls:**
   - One store daemon owns the node's data.
   - Full-text search is built into the engine, not taken from `tantivy`.

Three surveys (2026-09-26) sized the work.

### What the node uses SQLite for

**Schema**
- 27 generated tables plus 4 the crate owns; two of the 27 are FTS5 virtual
  tables.
- 39 indexes, including one expression index on a JSON path and two partial
  indexes.
- Reconciled on every open against a generated v29 schema.

**Triggers (15):**
- 6 keep the FTS indexes in step;
- 3 keep `memory_tags` in step with each row's JSON `tags`;
- 6 fill the sync outbox, gated on `sync_flags.sync_enabled`, with payloads
  built by `json_object`.

Echo suppression (`sync/record.rs`, `sync/graph.rs`) depends on those
outbox rows being written in the same statement as the row, with monotonic
`AUTOINCREMENT` ids.

**Full-text search**
- FTS5 with the default `unicode61` tokenizer.
- `bm25()` feeds RRF fusion (`retrieval.rs`), which expects a score where
  lower means better.
- `snippet()` is used by wiki search.
- Queries are OR-joined quoted tokens (`fts.rs`).

**Vectors**
- Little-endian f32 blobs in `vec_embeddings`, keyed by `vec_chunks` on
  `memories.rowid`.
- Search is brute force; `usearch` (feature `ann`) only narrows the
  candidates.
- About 65 references depend on the rowid.

**SQL features**
- JSON1: `json_extract`, `json_each`, `json_object`, `json_set`.
- Prefix `LIKE`, `EXISTS` and `IN` subqueries, and `GROUP BY` at 7 sites.
- 13 `ON CONFLICT DO UPDATE` sites, plus `INSERT OR IGNORE`/`OR REPLACE`.
- `julianday` and `strftime`.
- One scalar function: `effective_vitality`.

**Concurrency**
- WAL mode.
- One mutex-guarded connection per process, plus secondary connections for
  the sync worker, the peer server, the scheduler, the watcher and the
  promotion nudge.
- Several processes at once: one `rusty-remind-me server` per Claude Code
  session, the CLI behind hooks and `/remember`, and the `api` and `remote`
  daemons.
- Almost no explicit transactions: each statement, with its triggers, is the
  unit of atomicity.

**Other**
- Online backup (`backups/`, 10 kept), used by the pre-migration snapshot.
- `PRAGMA database_list` finds the file at 7 sites.
- Two importers read foreign SQLite files read-only: daily-backup-system and
  ChromaDB (mempalace).

**Scale**
- About 400 statement sites across 43 core files.
- About 250 functions take `&Connection`.
- About 826 tests open `Database::open_in_memory()`.
- The live store holds about 15k memories.

### What the engine has

**Present**
- Records of any serde type, including floats and options.
- Equality indexes.
- Ordered keyset and range indexes, in memory and rebuilt at open.
- A fsync'd insert log.
- Group commit.
- Crash-safe compaction.
- Guarded replace.

**Absent**
- Full-text search.
- Vector search.
- Uniqueness constraints.
- Crash-atomic writes across records or stores: `with_exclusive` isolates
  but does not roll back. The redo journal and MVCC live only in the
  `rusty_multimodal_db` app's server, which a node cannot depend on under
  the monorepo's layering rules (its ADR-0003).
- A data-directory lock.
- Access by more than one process.

Everything lives in RAM, which is fine at this scale. Even at 15k memories
with 16 chunks of 384-dim vectors each, the store is under a gigabyte, and
the real figure is far smaller.

**Nearby crates do not close the gap**
- `rusty_search` gets BM25 only from `tantivy` or C SQLite.
- `rusty_rusqlite` is in-memory only, with no FTS, JSON1 or triggers.
- `rusty_sqlite` wraps C SQLite.

### What retiring Python frees

No node feature depends on Python (`gap-analysis.md`: the gap table is
empty). What goes with it:
- the daily schema-drift CI and its three scripts;
- the generated-schema contract;
- the frozen triggers;
- the "revisit if drop-in interop stops being the point" clauses across
  ADRs 0002, 0004, 0005, 0007, 0016 and 0022.

It also unblocks ADR-0022 step 6.

## Decision

The node's storage moves to `rusty_multimodal_db_engine`, in the same shape
the hub's move took:
1. build and prove each piece beside the SQLite store;
2. copy the data over;
3. switch;
4. then delete.

Seven decisions follow.

### 1. The store becomes a Rust API, not a schema

Every table group goes behind a repository type in `remind_me_core::db`,
continuing ADR-0022. Callers stop taking `&Connection` and take a store
handle instead. Two things move out of SQLite and into the repositories, in
Rust:

- **Triggers.** Each repository write updates its derived data and records
  its outbox entry. Derived data covers the FTS index, the tag index, and
  the text the vectors are built from.
- **Echo suppression.** It becomes a flag on the write ("this came from
  sync; don't queue it"), not a scan of the outbox by id.

This happens while SQLite is still the store, so every behaviour is proven
on the old backend before the new one exists. The seam gets one trait,
`NodeStore`, at the point where a second backend appears. That backend is
the engine, so this is not speculative.

### 2. One store daemon owns the data

The engine keeps the store in one process's memory. So one long-lived
process, `rusty-remind-me daemon`, owns the data directory and holds its
lock. Everything else becomes a client:
- the MCP stdio server started per Claude Code session;
- the CLI (`remember`, `recall`, hooks);
- `api` and `remote`.

How the daemon works:
- **Starting:** the first client starts it if it is not running, and waits
  for it to be ready.
- **Protocol:** the daemon speaks the node's own operations (one request per
  store call), not SQL.
- **Transport:** loopback TCP with a token file in the data directory, mode
  600, like `remote`'s connector token. The node runs on Linux, macOS and
  Windows, and loopback TCP works on all three without platform-specific
  IPC.

The daemon is built **on the SQLite store first**. That separates two
risks: the process model is proven with the storage unchanged, and then the
storage changes with the process model unchanged.

### 3. The engine gains what the node needs

Four additions, each in `rusty_multimodal_db_engine`, each tested on its
own.

**a. A full-text index**
- The tokenizer follows FTS5's `unicode61` rules: Unicode letters and
  numbers are token characters, case folds, and diacritics are removed.
- Scoring is BM25 as FTS5 computes it (k1 = 1.2, b = 0.75, FTS5's IDF),
  returned in FTS5's sign convention so `retrieval.rs` needs no retuning.
- Snippets are built from token offsets.
- The index lives in memory and is rebuilt from the records at open, like
  the ordered indexes. It is derived data, so it needs no durability of its
  own.
- It is tested differentially: the same corpus and queries go through FTS5
  and through the engine index. Rankings must match, and scores must match
  within float tolerance.

**b. Crash-atomic write batches across stores**
- A small redo journal: a batch's records for several stores are written
  and fsync'd as one entry, then applied.
- On open, a complete entry is replayed and a torn one dropped.
- This replaces "the statement and its triggers are one unit": a memory, its
  tags and its outbox entry land together or not at all.
- It follows the design of the server's journal (`rusty_multimodal_db`'s
  ADR-0025),
  written fresh in the libs layer, since the app crate cannot be a
  dependency.

**c. Durable sequence allocators**
- Monotonic `u64` counters, persisted through the same journal.
- Outbox ids and chunk ids are allocated from them.

**d. A data-directory lock**
- The engine gets the `try_lock` the hub already takes, so a second daemon
  refuses to start.

No SQL is added. Joins, group-bys, `EXISTS` filters and JSON-path lookups
become repository code over records and indexes, as the hub's counts
already are. The expression index on `metadata.normalized_from` becomes an
equality index on a field extracted at write time.

### 4. Vectors are keyed by memory id

- Chunks are keyed by `(memory id, chunk index)`, not by `memories.rowid`,
  so nothing depends on a row number the engine doesn't have.
- Embeddings stay f32 records.
- Search stays brute force over the in-memory chunks.
- The optional `usearch` sidecar keeps narrowing the candidates, keyed the
  same way.

### 5. The data is copied once, keeping every id

A copy tool, `rusty-remind-me copy-store`, reads `memory.db` read-only and
writes the engine data directory. As with the hub copy, three rules apply:
- it verifies every row after writing;
- it refuses rows the engine cannot store rather than dropping them
  silently;
- it never writes to the source.

The daemon runs the copy on first start when it finds a `memory.db` and no
data directory. It leaves `memory.db` in place.

### 6. Foreign SQLite files are still read, read-only

The daily-backup-system and mempalace (ChromaDB) importers read other
programs' SQLite databases. That is reading a file format, not storing
data. It stays on `rusqlite`, read-only, behind the importers' existing
features, until a hand-written read-only SQLite page reader replaces it.
That replacement is out of scope here and is noted as a follow-up.

So the node stops storing in SQLite. It stops linking SQLite only once
those two importers are gated off or rewritten.

### 7. Order of work

| Phase | What | Ships |
|---|---|---|
| 0 | **Retire Python.** Drop the schema-drift CI and scripts. Rewrite Tenet 3. Make the `schema_*.sql` files hand-owned. Mark `CUTOVER.md` and `gap-analysis.md` historical. Fix `configure_mcp.py`'s stale path. | Docs and CI only |
| 1 | **Finish the seam.** Put every remaining table group behind a repository, then move the triggers and echo suppression into the repositories (FTS, tags, outbox). Re-key vectors on memory id. | Several PRs, one group at a time, SQLite still underneath |
| 2 | **The daemon.** `rusty-remind-me daemon` on SQLite; MCP, CLI, hooks, `api` and `remote` become clients; auto-start. Opt-in first (2a), then the default (2b). | Behaviour-preserving |
| 3 | **Engine additions.** Full-text index, write batches, sequence allocators, directory lock (§3). | Engine PRs, each with its own tests |
| 4 | **The engine-backed store.** Callers take a `NodeStore` handle instead of `&Connection`. `NodeStore` on the engine, run by the same test suite as the SQLite store, plus the copy tool (§5). | Behind a feature, off by default |
| 5 | **Switch.** The engine becomes the default and the daemon copies on first start. | Default switch |
| 6 | **Remove.** Delete the SQLite store and its schema. | Removal |

Each phase is merged green before the next starts. Phase 1 is by far the
largest: about 400 statement sites.

### How the tests carry over

About 826 tests build a store with `Database::open_in_memory()`. That
constructor stays, and it becomes the seam: once `NodeStore` exists, it
returns a store of whichever backend the test run selects (an environment
variable, as `REMIND_ME_HUB_TEST_DATABASE_URL` did for the hub). CI runs
the whole suite on both backends until phase 6.

That is the node's version of the hub's differential test: the same
assertions, two stores. The SQLite store is kept alive through phase 5 as
the reference.

## Consequences

**Gains**
- The node is one process that owns its data, with the same in-memory
  engine as the hub.
- Search, sync capture and storage are Rust the project owns, with no
  schema to keep identical to anything.
- Triggers no longer do work the code can't see.

**Costs**
- **A daemon.** A crash takes every session's memory access with it until
  the next client restarts it. A client that can't reach it fails clearly.
  Clients no longer open the store themselves.
- **No SQL.** Ad-hoc inspection with `sqlite3 memory.db` goes. A `dump`
  command is a likely follow-up.
- **Work:**
  - about 400 SQL sites rewritten as repository code;
  - two engine subsystems to write and prove (full-text search and write
    batches);
  - a copy that has to be right the first time for each user.
- **One-way.** After the switch, a node's data is not SQLite. Going back
  means a copy the other way, which is not planned.

**What stays SQLite-shaped**
- Nothing in storage.
- The two foreign-file importers read SQLite until their reader is
  replaced (§6).

## Alternatives declined

- **Keep a v29 `memory.db` mirror for Python.** It keeps SQLite, and the
  schema-parity burden, in the node. The owner retired Python instead.
- **`tantivy` for search.** A large new dependency and a second on-disk
  index to keep in step with the store. The owner chose an engine-native
  index.
- **An exclusive lock with no daemon.** A second Claude Code window, or a
  hook running during a session, would fail to open the store.
- **Talk to a `rusty_multimodal_db` server over its wire protocol.** Its
  value types have no floats or nulls, and its queries are too narrow for
  the node. It would add a network service to run beside every node.
- **Port the app's journal and MVCC by depending on the app crate.**
  The monorepo's ADR-0003 forbids an apps-to-apps edge across families.
  ADR-0021 declined an exception, and the write batch in §3b is smaller
  than MVCC.
- **Switch the node in one change.** The hub's move worked because each
  step was proven before the next; the node is ten times larger.

## Progress

### Phase 0: done

The Python reference is retired. The drift CI and its scripts are gone, the
schema files are hand-owned, and Tenet 3 is replaced.

### Phase 1: the steps

Phase 1 goes one group per change, as ADR-0022 did. Every step keeps the
schema, the SQL and the public signatures, so the whole suite passes
unchanged after each.

| Step | Group | Repository | State |
|---|---|---|---|
| 1 | Memory row writes | `db::memories::Memories` | Done |
| 2 | Entities, relations and mentions | `db::entities::Entities` | Done |
| 3 | Wiki pages, links and their index | `db::wiki::WikiIndex` | Done |
| 4 | Vectors, re-keyed on memory id (§4) | `db::vectors::Vectors` | Done (schema v30) |
| 5 | Promotions, imports, archives and curation queues | `db::promotions::Promotions`, … | Done |
| 6 | The outbox, with echo suppression as a flag | `db::outbox::Outbox` | Done (the flag landed with step 7) |
| 7 | Triggers move into the repositories | `db::derived` | Done (schema v31) |
| 8 | Callers stop taking `&Connection` | a store handle | 8a done: no SQL outside `db::`; 8b moved to phase 4 |

Writes go first because the triggers fire on writes. Once each table's
writes have one home, step 7 moves its triggers there in one place.

**Step 1.** `db::memories` holds every insert, sync upsert and field update
of `memories` that domain modules used to write inline:
- `insert` and `insert_or_ignore` take a `NewMemory`, a whole row whose
  `new()` fills every column with the schema's default. So each writer names
  only what it sets, and the row matches what its old `INSERT` produced. A
  test holds `new()` to the schema defaults column by column.
- `upsert_synced` is the sync apply's `ON CONFLICT` write. It still keeps
  the local `created_at`, `doc_id` and `chunk_index`.
- Field updates: superseding (one memory, or every chunk of an import),
  a merge rewrite, vitality, access tracking, the ingest marker, and the
  sync merge's tags and metadata.

Eight writers moved: `add_memory`, capture (both halves and decomposed
facts), skeletons, promotion, normalization, the three importers, and sync
apply. The rules stay where they were: vitality seeding, provenance, what a
merge writes, and when `updated_at` is stamped. The only observable change
is that a normalized memory's copied tags are re-serialized rather than
copied as text, so `["a", "b"]` becomes `["a","b"]`. Both parse the same.

The remaining writes to `memories` are already in `db::`: `update_memory`,
deletes, bulk tagging, annotation and reclassification in `queries.rs`, and
the history, feedback and reminder repositories.

**Step 2.** `db::entities` holds every statement `entity.rs` and
`sync/graph.rs` ran against `entities`, `entity_relations` and
`memory_entities`:
- lookups, the resolve scan, the listing page and the profile's facts and
  linked memories;
- inserting and merging entities, mention links and relations;
- the traversal's per-hop edge query;
- id renormalisation's rename, merge, delete and repoint;
- the sync apply's view, upsert and alias merge.

The rules stay in `entity.rs` and `sync/graph.rs`: name normalisation and
derived ids, "existing kind wins", alias union order, the traversal's
frontier and cap, and LWW for synced entities. The contradiction check's
read of live triples moved to `db::memories`. Nothing outside `db::` writes
to any of the four tables now. Reads of the graph from `contradictions.rs`,
`export.rs`, `expansion.rs`, `promotion.rs` and the sync server stay for
their own steps.

**Step 3.** `db::wiki` holds every statement `wiki.rs` and `wiki_fs.rs`
ran:
- page upserts, both the file-backed one and the unbacked one used by
  imports, which keeps a cached mtime;
- lookups, listings, the generated index's titles and summaries, and the
  reconcile pass's cached mtimes;
- link replacement and removal;
- the FTS5 search, with its BM25 order and snippet;
- `wiki_meta`.

The compile brief's two reads of new memories moved to `db::memories`. The
rules stay in `wiki.rs` and `wiki_fs.rs`: files are the source of truth,
reserved slugs, the reconcile's mtime test, the load budget and the
watermark. One small change: deleting a page by slug through
`delete_wiki_page` now also clears its outgoing links, as the file-backed
delete always did. Nothing reads those links outside tests.

**Step 5a.** `db::promotions` holds the `promotions` table, which it now
creates at open, and every read `promotion.rs` ran:
- the three rungs' candidate queries and their uncapped counts, sharing one
  predicate each, so a listing and its count cannot disagree;
- source checks, the duplicate-promotion lookup and recording provenance;
- both directions of provenance, surviving sources, and the persona and
  demoted listings.

Loading a candidate's source memories moved to `db::memories::get_many`.
The rules stay in `promotion.rs`: which rung reads which category, the fact
threshold, the persona floor, and demotion as a read-time judgement.

**Step 5b.** Import bookkeeping has two repositories:
- `db::archives::Archives` holds `import_archives` and
  `import_archive_spans`, which it now creates at open. It records
  archives and spans, looks up a memory's source span, forgets an import,
  counts a blob's references, and lists archives for pruning.
- `db::imports::ImportLedger` holds `chat_imports`, `dbs_imports` and
  `mempalace_imports`. It covers recording, the already-imported lookups,
  the undo's per-kind memory queries (tracked and untracked mempalace
  content alike), and forgetting tracking rows. A chat import still loses
  its row only once nothing of it is left.

Reading the foreign SQLite files the dbs and mempalace importers take in
stays with them (§6). The rules stay in `archive.rs`, `undo_import.rs` and
the importers.

**Step 5c.** `db::curation::Curation` holds the curation queues' reads:
- captures awaiting decomposition, a capture's rows and tags, and capture
  activity;
- raw imports awaiting normalization, and what a normalization copies;
- the four maintenance backlog depths, named by a `Backlog` enum;
- contradiction candidate pairs, their count, shared entity names and
  sides.

These came from `capture.rs`, `normalize.rs`, `maintenance.rs` and
`contradictions.rs`, which keep the rules: snippet lengths, batch bounds,
import sources, the fan-out ceiling, and the keyset cursor. The maintenance
counts keep their own SQL, which is cheaper than the batch queries' and
must count the same rows (a test holds the two together for the capture
and import backlogs). Consolidation's candidate read joins the vector
tables by rowid, so it moves with step 4.

**Step 6.** `db::outbox::Outbox` holds every statement against
`sync_outbox` and `sync_sends` from the sync modules:
- the push batch and the per-remote pending count;
- the outbox's size;
- pruning;
- the clear and backfill that follow sync being switched off or on;
- echo suppression's high-water mark and mark-as-sent.

Echo suppression is still the old technique: take the high-water mark
before a synced write, then mark that key's rows above it as sent. It can
only become a flag on the write once the repositories, not triggers, write
the outbox rows, which is step 7. The peer server's pull pages and the
sync count endpoints read `memories` and the graph directly; they move in
step 8 with the other readers.

**Step 4.** Schema v30 keys vectors by memory id. `vec_chunks` is now
`(memory_id, chunk_ix, embedding)` with that pair as its primary key.
`vec_embeddings` and the rowid index are gone.

On open, `migrations::rekey_vectors` moves a v29 database over:
- it spots the old shape and renames the old table aside;
- it creates the new one from the schema file;
- it copies every chunk whose memory still exists;
- it drops the old tables.

All of that runs inside one savepoint, before the generic rebuild, and
after the pre-migration backup. A chunk whose memory is gone is dropped:
under v29 it could only have been inherited by the next memory to reuse
its rowid.

`db::vectors::Vectors` holds every vector statement: storing, deleting,
the semantic scan (narrowed by memory ids when the ANN index proposes
some), the unembedded list, consolidation's candidates, and
`embedding_meta`.

Three knock-on changes:
- The ANN sidecar manifest now keys vectors by memory id under a `v2` tag,
  so an older manifest reads as unusable and search falls back to the full
  scan until `rebuild`.
- `ApplyOutcome::Applied` no longer carries a rowid.
- Opening a database stamped by a newer build is now refused, because
  reconciliation would reshape it backwards.

Tests build the v29 layout by hand and check the carry-over, the orphan
drop, idempotence, and the refusal.

**Step 7.** Schema v31 has no triggers. `db::derived` does what the
fifteen triggers did, and every write in `db::` goes through it.

`write_memory(id, origin, write)` runs a write to one memory inside a
savepoint:
- it takes the row out of the full-text index as it was;
- it runs the write;
- it puts the row back and rebuilds its tag rows, or drops them with the
  row;
- for a local write, it queues an outbox `insert` when the write created
  the row, or an `update` when it moved `updated_at`. That is the old
  `memories_outbox_au` guard, so access tracking still queues nothing.

A write that can touch several memories (superseding an import, deleting a
capture's skeleton, stamping an ingest marker, the v29 refile) selects the
ids first and writes each in turn. Entity, relation and link writes queue
their own rows. Wiki writes keep `wiki_fts` in step the same way.

**The payloads keep the trigger shape.** They are still built by SQLite's
`json_object` from the row as written, with the triggers' column lists, so
tags and metadata travel as JSON text and `sensitive` as 0 or 1. A peer or
the hub reads exactly what it read before.

**Echo suppression is now `Origin::Sync`.** Sync apply passes it, and
nothing is queued. Before, the triggers queued the echo and the apply
marked it sent after a high-water mark. `Outbox::high_water` and
`suppress_echo` are gone with that.

**Opening an older database** drops the fifteen triggers by name. They hold
no rows, and left in place they would index and queue everything twice.
`db::derived::rebuild_indexes` rebuilds the indexes from the rows. Tests
that plant rows with raw SQL call it, and it doubles as a repair.

**Step 8a.** No statement against the node's own store is left outside
`db::`. The last readers moved:
- `db::related::Related` has expansion's reads (shared entities, document
  window, co-retrieval) and its association bump.
- `db::sync_feed::SyncFeed` has the peer server's four keyset-paged pull
  feeds and graph counts, as the wire records the server returns.
- `StoreStats` gained the memory totals, tombstone counts, the by-category
  count reconciliation uses, and the digest's shareable-recent reads.
- `Memories` gained the export filter, the live list the vitality report
  walks, and the code-reference scan.
- `Entities` gained the export's link and relation dumps.
- `db::database_path` replaces six copies of `PRAGMA database_list`.

The only SQL left outside `db::` reads the foreign SQLite files the dbs and
mempalace importers take in (§6).

**Step 8b moves to phase 4.** Replacing `&Connection` with a store handle
touches about 100 public functions in core, 40 in the api, mcp, cli and
remote crates, and 88 test files with some 370 raw-SQL sites. Done now, the
handle would only wrap a `Connection`: an abstraction with one backend.
Phase 4 introduces the engine-backed store, so the trait is shaped once,
against two real implementations, and callers change once. With 8a done,
phase 1 is complete.

### Phase 2: the daemon

**2a, done: the daemon, opt-in.** `rusty-remind-me daemon` owns the
SQLite store behind `REMIND_ME_DAEMON=1`. MCP over stdio, the CLI's store
commands, `api` and `remote` become its clients, and the first one starts it.
Three points §2 left open were decided while building it:

- **Settings are split.** Before the daemon, each MCP client ran its own
  process under the environment its config gave it, about 85 `REMIND_ME_*`
  variables. One daemon cannot serve them all under one environment without
  silently changing some. `REMIND_ME_CLIENT`,
  `REMIND_ME_DEFAULT_RESPONSE_FORMAT` and `REMIND_ME_TOOL_PROFILE` travel with
  each connection and apply to it alone (`daemon::session`), as does the MCP
  handshake's client identity. Every other `REMIND_ME_*` variable belongs to
  the store: each client sends a SHA-256 fingerprint of its values, and on any
  difference it announces why and runs in-process as before
  (`daemon::settings`). The comparison errs wide: a variable compared needlessly
  costs a fallback, a variable wrongly skipped would be served under another
  client's value.
- **A different build is refused.** A daemon outlives the binary that
  started it, so after an upgrade the old code would keep serving, perhaps
  against a newer schema. The handshake compares a build id (version plus
  the executable's size and modification time) and the client falls back on
  a mismatch; `daemon stop` accepts any build.
- **The protocol has four modes.** One JSON hello line with the token opens
  each connection. MCP JSON-RPC then flows one line per request and exactly
  one line back (`null` for a notification). The CLI sends typed store
  operations (`daemon::ops`) and gets back the same typed values, so its
  output is byte-identical. The dashboard API's HTTP bytes are relayed to an
  `ApiServer` inside the daemon. A control mode carries only `status` and
  `shutdown`.

On Windows a child inherits every inheritable handle, so a daemon started by
an MCP client would hold that client's stdout pipe open after the client
exits. The client clears inheritance on its standard handles before starting
the daemon. That is a second use of the FFI `windows-sys` dependency ADR-0013
admitted, not a new dependency.

**2b, next: on by default.** Once 2a has run on real machines, the daemon
becomes the default with `REMIND_ME_DAEMON=0` as the way out. Phase 5 needs it
on, since the engine keeps the store in one process.

### Phase 3: engine additions

**3a, done: the journal, sequences and directory lock** (§3b, §3c, §3d),
in `rusty_multimodal_db_engine`:

- `journal::Journal` commits a batch of whole-value puts and deletes across
  named stores as one `fsync`'d entry before the caller applies it, replays
  every batch since the last checkpoint on open, and checkpoints once the
  caller's stores have synced. Entries carry a CRC-32 as well as a length, so
  a last entry whose length landed but whose bytes did not is dropped as torn
  rather than refused as corrupt; a bad entry before the last is refused.
  Replay needs no record of what already landed: every change is a whole
  value or a delete by id, so applying it twice is applying it once.
- Sequences ride on the journal: every entry carries every counter, so a
  value is durable with the batch that uses it, and a checkpoint keeps them.
  A value issued but never committed can be issued again after a crash,
  which is safe because nothing durable holds it.
- `dir_lock::DirLock` is the hub's directory lock, moved into the engine;
  the hub now takes it from there.
- `tests/journal_batches.rs` runs the node's case over two real engine
  stores: a memory and its outbox entry, crashed between the two, both
  present after reopen; a batch replayed over stores that already hold it;
  outbox ids never reissued across crashes and checkpoints.

**3b, done: the full-text index** (§3a), `fulltext::FullTextIndex`:

- `unicode61` is ported from `fts5_tokenize.c` and `fts5_unicode2.c`, with
  its category, case-fold and diacritic tables generated from the bundled
  `sqlite3.c` rather than re-derived from Unicode data. Deriving them from
  Rust's own Unicode tables would drift whenever the two Unicode versions
  differ, and a test pins the tables to the SQLite that links.
- Documents have a fixed number of columns (a const generic, so a wrong
  count does not compile). Queries are any of several phrases, the only
  shape the node builds (`fts::sanitize_fts_query`), and a phrase matches
  consecutive tokens in one column, which is how `memory_tags` matches.
- `bm25()` is FTS5's to the formula and the order of operations: a row's
  size is its tokens across all columns, each phrase has its own IDF and
  frequency (a repeated phrase counts twice), and the IDF floor is `1e-6`.
- `snippet()` is `fts5SnippetFunction` line for line: window scoring, the
  sentence-start bonus, coalesced highlights and the ellipses.
- `tests/fulltext_vs_fts5.rs` runs a seeded corpus (prose, mixed case,
  underscores, accents, Greek and CJK, JSON tag arrays, edits and deletes)
  and 600 queries through FTS5 and the index: the same rows in the same
  order, scores within 1e-9, and identical snippets in five call shapes.
  Mutating the BM25 `b`, the sentence heuristic, or the snippet window start
  each fails it.

Phase 3 is complete.

### Phase 4: the engine-backed store

**4a, done: the seam** (`db::store`). Every caller takes `&Store<'_>`
instead of `&rusqlite::Connection`, and every repository returns
`db::Result<T>`, whose error is the backend-neutral `StoreError`
(`NotFound`, `Invalid`, `Sqlite`). SQLite is still the only backend, so
this is a pure refactor with no change in behaviour.

- The handle is called `Store`, not the `NodeStore` §7 planned. The two
  backends are a closed, temporary pair (SQLite leaves in phase 6), so
  `Store` is an enum each repository matches on, not a trait object.
  Adding the engine is then a variant plus a match arm per repository,
  and nothing outside `db::` changes.
- `Database::store()` replaces `Database::conn()`. The store holds the
  database lock for as long as it lives, as the guard it replaces did.
  `Store::over_sqlite(&conn)` wraps a connection opened elsewhere: a
  worker's own connection, an importer's transaction, or a test's.
- `Store::sqlite()` is the one way back to SQL. The schema and its
  migrations, backup, and tests that inspect rows directly use it. It
  returns `None` once a store is backed by something else, so a SQLite-only
  path fails as a typed error (`BackupError::NotSqlite`), not a panic.
- Crate-level error enums (archive, export, promotion, skeleton, vectors,
  sync, pid, backup, and the importers) wrap `StoreError` instead of
  `rusqlite::Error`. `StoreError::NotFound` keeps SQLite's "Query returned
  no rows" message, which callers and tests match on.

**4b, done: the engine backend, and saved searches on it.**

- The `engine-store` feature (off by default) adds `db::engine`.
  `EngineTables` holds one engine store per table moved so far, plus the
  data-directory lock (`node.lock`). A `Store` carries the tables beside
  its SQLite connection. Each moved repository answers from the engine when
  the tables are present, and every other group stays on SQLite until its
  own step, so a partly moved store is a working store.
- `Database::open_in_memory()` attaches temporary engine tables when
  `REMIND_ME_STORE=engine`; their directory is removed on drop. A CI leg
  (`rusty_remind_me feature engine-store`) runs the whole core suite that
  way, so every test that touches a moved group runs on the engine.
  `Database::open(path)` stays on SQLite until the copy tool exists (§5).
- Record ids follow the hub's layout: a UUID v5 of the node's string id,
  in a node namespace, with the string kept on the record so a collision is
  refused rather than merged. JSON columns stay JSON text, since the engine
  encodes records with bincode.
- The first group is saved searches (`saved_searches`,
  `saved_search_seen_memories`). It has one caller module and no
  cross-group writes. The unique name becomes the equality index, checked
  before each insert; the seen rows are indexed by saved search; `INSERT OR
  IGNORE` becomes a check by the pair's derived id. The repository's unit
  tests run every case on both backends in one pass, and the integration
  test that counted seen rows in SQL now counts them through the repository.
- A failure the engine reports is `StoreError::Engine`.

**4c, done: import archives on the engine.**

- `import_archives` and `import_archive_spans` move to the engine. Their
  one caller is `crate::archive`, and they touch no other group.
- An archive is keyed by its import id and indexed by blob hash, which
  `count_with_hash` counts; a span is keyed by its memory id and indexed
  by import, which `remove` walks. `INSERT OR REPLACE` becomes replace-if-
  present, and `span_source` keeps the inner join's rule: a span whose
  import has no archive has no source.
- `oldest_first` orders by the `archived_at` text as SQLite compares it,
  with ties in import-id order where SQLite leaves them unspecified.
- The repository gains `span_count`, so the integration tests stop reading
  the tables with SQL. Those tests now open an in-memory database, so the
  engine CI leg runs them against the engine.

**4d, done: background threads share the engine tables.**

- Every group still on SQLite is written from a background thread too: the
  sync worker writes `sync_log`, the scheduler `reminder_deliveries`, and
  the folder watcher and the sync paths write memories. Each thread opened
  its own SQLite connection and wrapped it with `Store::over_sqlite`, which
  carries no engine tables, so once a group moved, the thread would have
  written SQLite while the main store read the engine.
- The fix keeps each thread's own connection and shares the engine tables
  with it. `Database` holds its `EngineTables` behind an `Arc`
  (`EngineHandle`). A `SecondarySource` (the database file plus that
  handle) comes from `Database::secondary_source()` or
  `Store::secondary_source()`, and `SecondarySource::store(&conn)` gives a
  thread a store over its own connection with the shared tables beside
  it. The engine tables lock per call, so a thread still holds no
  process-wide lock across network I/O, the reason it has its own
  connection.
- The scheduler, folder watcher, promotion nudge, sync worker and sync
  peer server all take a `SecondarySource`. The dbs and mempalace
  importers, which run a SQLite transaction of their own inside a store
  call, use `Store::sharing_engine(&tx)`; engine writes made there are not
  part of that transaction, which matters only once those importers write
  a moved group, and the journal (§3b) addresses it then.
- Chosen over removing the second connections in favour of one store for
  every thread: that would mean restructuring the sync worker so network
  I/O runs outside the store lock, a larger change for the same result.
  Once SQLite goes, a `SecondarySource` is just the engine handle.

**4e, done: `sync_log` on the engine.**

- Each remote's pull cursors and liveness stamps move to the engine, keyed
  by remote id. `sync_flags` and `sync_sends`, the other two tables
  `SyncState` owns, stay on SQLite: `db::derived`'s outbox gate reads
  `sync_flags` inside its SQL, and `db::outbox`'s batch query joins
  `sync_sends`. They move with those groups.
- Every write is a read-modify-write of the remote's whole row, as the
  SQL's `INSERT … ON CONFLICT DO UPDATE` is column by column; a remote
  with no row starts from the schema's defaults (epoch timestamps, empty
  keyset id, `hub_seq` cursor -1). `reset_pull_cursors` stays an
  `UPDATE`: no row, no write.
- The sync worker writes these rows from its own thread, which 4d made
  possible.
- `SyncState` gains `remote_row` and `put_remote_row`, a whole-row read
  and write over `SyncLogRow`. The copy tool (§5) writes rows whole, and
  the sync tests seed states no sync cycle produces; five integration
  test files stop reading and writing `sync_log` in SQL.

**4f, done: the journal's id sequences, and analytics snapshots on them.**

- `EngineTables` opens the redo journal (`node.journal`) beside its
  stores. For now it carries only the durable id sequences (§3c): no
  cross-store batch is written yet. A journal whose replay holds changes
  is refused at open, since only a newer build could have written one.
- `next_id(sequence)` allocates the next value, commits it, and
  checkpoints, all before the record that uses it is written. A crash in
  between leaves a gap in the ids, never a reissued one. At open, each
  sequence is raised to the highest id its table already holds, so rows
  written by anything else (the copy tool) are never collided with.
- `analytics_snapshots` is the first table on it. Its engine id is the
  snapshot's integer id. The day index holds `date(captured_at)` as SQLite
  computes it, the UTC calendar day, so `snapshot_on` agrees across
  backends for captures made east or west of UTC. The two maps stay JSON
  text, decoded by one shared helper.
- `reminder_deliveries` was the other candidate. It stays on SQLite: the
  scheduler's reminder query tests it in a `NOT EXISTS` inside a `LIMIT`ed
  query on memories, so it moves with memories.

**4g, done: memory revisions on the engine.**

- `memory_revisions` moves to the engine, on the journal's
  `memory_revisions` sequence: a revision id is what a caller passes back
  to revert, so it must never be reissued. The table is indexed by
  memory; `list` orders by the `edited_at` text newest first, then id,
  as the SQL does, so a burst of edits in one clock tick still lists in
  write order.
- `Revisions`' reads and writes of `memories` (is the memory live, its
  current tracked values, writing a revert back) stay on SQLite.
- The revision is written before the memory's `UPDATE`, as before. The two
  were already separate statements, not one transaction, so a crash
  between them leaves what it always could: a revision equal to the
  memory's current values, which a revert would report as no change. The
  journal (§3b) makes the pair atomic once memories move.

**4h, done: the wiki on the engine, with the engine's full-text index.**

- `wiki_pages`, `wiki_links` and `wiki_meta` move to the engine, and
  `wiki_fts` becomes the engine's full-text index (§3a): in memory, built
  from the page records when the tables open, and updated by every page
  write in the same call, as `write_wiki_page` keeps `wiki_fts` in step.
  None of the wiki's statements touch memories, so it moves whole.
- The search takes phrases, not an FTS5 expression. `fts::query_phrases`
  splits a query once; SQLite builds its `MATCH` from them
  (`fts::match_expression`, which `sanitize_fts_query` now wraps) and the
  engine builds `Query::any_of`, so both search for exactly the same
  thing. A differential test runs one corpus through both and requires
  the same pages, order and snippets.
- Orderings follow the SQL: `updated_at` text newest first, and
  `COLLATE NOCASE` as ASCII-only case folding. Where SQLite leaves ties
  unspecified (equal BM25 scores, equal sort keys), the engine breaks
  them by slug.
- This is the last group that touches no memories. `promotions`, the
  import bookkeeping tables, the vectors, `reminder_deliveries`,
  `memory_feedback`, `sync_flags` and `sync_sends` are each read in SQL
  alongside `memories`, so the rest of phase 4 is memories and what
  joins them, planned before it starts.

**The memories core: built dark, then switched on.** Memories and every
group that joins them move in five PRs. The first four build the core on
the engine behind a gate that only the core's own tests open, so nothing
changes for a running node or for the engine CI leg until the fifth
switches it on:

1. memory writes, with tags, full-text, the outbox, `sync_flags` and
   `sync_sends`, as the first real journal batches;
2. reads: search, list and get (with an FTS5 differential test), stats
   counts, `reminders` and `reminder_deliveries`, feedback;
3. the graph: entities, relations, mentions, associations, the sync feed,
   curation, promotions;
4. vectors and the import bookkeeping, with the importers' transactions
   as journal batches;
5. switch-on.

**Core 1, done: memory writes on the engine, built dark.**

- `EngineTables` gains an optional memories core (`db::engine::core`):
  `memories`, `sync_outbox`, `sync_sends` and `sync_flags`, plus the
  full-text index over (content, category, tags) and the tag index, both
  derived from the rows at open like the wiki's and never stored. Only
  `EngineTables::open_with_core` opens it, and only tests call that, so
  `Store::core()` is `None` everywhere else and those repositories stay on
  SQLite.
- A row keeps every column as SQLite stores it: tags and metadata stay
  JSON text, nullable columns stay optional. The tag index holds exactly
  what `json_each(tags)` yields as text, including an object's values and
  a lone string.
- Every memory write is a closure from the stored row to keep, put or
  delete (`engine::memories::write`), mirroring `derived::write_memory`:
  the memory and the outbox entry recording it (an `insert` for a new
  local row, an `update` when a local write moves `updated_at`, only
  while `sync_enabled` is `'1'`) go into one journal batch (§3b). The
  payload has the trigger shape's 28 keys, and a test compares it with
  `json_object`'s as JSON.
- `EngineTables::commit` makes the batch durable, applies it (a put
  inserts or replaces, a delete of a missing record is a no-op, so a
  replay is idempotent), keeps the derived indexes in step, and
  checkpoints. If applying fails after the batch is durable, the tables
  refuse every later write until reopened, since a checkpoint would drop
  the unapplied batch; the reopen replays it. The journal now replays core
  batches at open instead of refusing every non-empty batch. It still
  refuses a store it does not know, and tables opened without the core
  refuse any core batch.
- Outbox ids come from the journal's `sync_outbox` sequence, allocated
  into the same batch as their entry, and are raised at open to the
  highest id stored. Prune, clear, backfill and a batch of sends are each
  one batch.
- `Memories`, `Outbox` and `SyncState`'s flags and sends dispatch to the
  core. The graph's rows stay on SQLite, but `queue_entity`,
  `queue_relation` and `queue_link` select their payload from SQLite and
  queue it into the engine's outbox (`derived::GraphOutbox`), and the
  backfill does the same for entities and links.
- Not yet on the core: the memory writes made by raw SQL inside
  `queries.rs`, `feedback.rs`, `history.rs`, `reminders.rs` and the
  migrations. Each sits in a function that also reads memories in SQL,
  so it moves with those reads in core PR 2.
- A differential test runs every write `Memories` makes, with sync on, on
  both backends. It requires the same rows, reads, counts and outbox
  payloads. Where SQLite leaves an order unspecified (scan order, ties),
  the engine sorts by `created_at`, then id.

**Core 2a, done: get, list and the field edits on the core.** Core PR 2
ships in three parts so each stays reviewable: 2a (this), 2b search, 2c
stats, reminders, feedback and history.

- `queries.rs` no longer speaks SQL about memories for get, list,
  update, delete, bulk tag, annotate, reclassify and the unclassified
  batch. Each goes through a new `Memories` method (`get_live`,
  `live_category`, `list_page` with a `ListFilter`, `apply_edit`,
  `delete_live`, `of_type_page`), which dispatches to the core when it
  is present.
- One `MemoryEdit` carries every field edit: each `Some` field is
  written and `updated_at` is always stamped. Update, bulk tag, annotate
  and reclassify each build one, so their `UPDATE`s cannot drift apart.
- `list_page` keeps the SQL's order, `created_at` text newest first and
  ties by id descending, with filters before the count and the page.
  `of_type_page` takes the content's first 500 characters, as `substr`
  counts them.
- `unannotated_batch` stays in SQL: it joins `memory_entities`, so it
  moves with the graph (core PR 3). `update_memory` still records its
  revision through `history.rs`, which reads memories in SQL, until 2c.
- A second differential test runs listings (filters, sensitivity,
  paging), edits, tombstones, hard deletes and the batch read on both
  backends and requires identical answers.

**Core 2b, done: search on the core.**

- The keyword half of search and the paged search no longer build SQL in
  `queries.rs`. They go through `Memories::keyword_hits` (with a
  `KeywordFilter`), `Memories::keyword_page` (with a `PageFilter` and an
  optional `EntityScope`) and `Memories::sensitive_ids`, which use the
  core's full-text index when it is present. Both backends take the
  query's phrases from `fts::query_phrases`, so they search for the same
  thing.
- The engine ranks by the same BM25 FTS5 computes. Ties, which SQLite
  left unspecified, now break by id on both backends, and the unranked
  newest-first page breaks `created_at` ties by id descending: the only
  change on SQLite.
- The dormancy filter stays inside the query, before the limit: SQLite
  calls the registered `effective_vitality` function, and the engine
  calls the same Rust function (`vitality::effective_vitality`), as of
  one instant for the whole search.
- The entity scope reads the linked memory ids from SQLite's
  `memory_entities` while the graph stays there (core PR 3).
  `lower(subject)` and `lower(object)` fold ASCII only, as SQLite's
  `lower` does.
- Vectors stay on SQLite until core PR 4, so on the core the semantic half
  of a fused search still reads SQLite.
- A differential test runs one corpus and a set of queries through FTS5
  and the core: ranked hits with scores, category, sensitivity and
  vitality filters, limits, paging, tags, the entity scope, and
  superseded and tombstoned memories. It requires the same hits, in the
  same order, with the same scores to ten significant digits.

**Core 2c, done: reminders, feedback and history on the core.**

- Two more tables join the core: `reminder_deliveries`, keyed by the
  (memory, `remind_at`) pair that is its unique index, and
  `memory_feedback`, keyed by its text id and indexed by memory. Both
  commit through the journal like the rest of the core.
- `Reminders`, `Feedback` and `Revisions` dispatch to the core for every
  statement that reads or writes memories or those two tables: whether a
  memory is live, setting `remind_at`, the reminder windows (with their
  `NOT EXISTS` over deliveries), recording a delivery, importance, the
  feedback log and the review queue (with its `NOT EXISTS` over
  feedback), and a revert's current and restored columns.
- The engine keeps the SQL's rules: `INSERT OR IGNORE` on a repeated
  delivery, the `PRIMARY KEY` and the `signal` `CHECK` on feedback, and
  `julianday` day counts for staleness. A never-read memory sorts
  first among equal weights, as SQLite sorts `NULL`.
- Ties the SQL left unspecified now break by id on both backends:
  reminders by `remind_at`, the review queue by weight and then last
  access. Feedback events list oldest first.
- Deleting a memory removes its feedback through the new
  `Feedback::delete_for`.
- With this, `update_memory` and `revert` work end to end on the core.
- Differential tests cover reminders (windows, limits, delivery,
  rescheduling, clearing, tombstones) and feedback and history (review
  queue and batch, events, refused ids and signals, importance, delete,
  revert with raw stored text). Each runs on both backends and requires
  identical answers.

**Core 2d, done: the stats counts on the core. Core PR 2 is complete.**

- `StoreStats` counts memories on the core when it is present: live
  memories, counts by category, source and tag, totals and tombstones,
  every memory by category, the shareable digest and the recent list.
- The chat-import count stays on SQLite until the import bookkeeping
  moves (core PR 4). The storage figures (file, size, schema version)
  stay on SQLite until the switch-on.
- The digest's newest-first list now breaks `created_at` ties by id,
  descending, as the recent list already did.
- A differential test runs every count on both backends, with tombstones,
  a sensitive memory, an empty category and multi-byte previews, and
  requires identical answers.

With 2a–2d, every read and write of memories outside the graph, the
vectors and the import bookkeeping goes through a repository that serves
it from the core.

**Core 3a, done: entities, relations and mentions on the core.** Core
PR 3 ships in three parts: 3a (this), 3b associations and the sync feed,
3c curation and promotions.

- The core gains `entities` (keyed by id), `memory_entities` (keyed by
  the memory/entity pair, indexed by entity) and `entity_relations`
  (keyed by id, indexed by subject). `Entities` dispatches every
  statement to them when the core is present.
- A graph write and its outbox entry commit as one journal batch, with
  the payloads `db::derived` builds on SQLite: an entity's insert or
  update, a new local mention link, a new local relation. So the graph
  no longer needs `GraphOutbox`'s SQLite select on the core, and the
  backfill takes entities and links from the core too.
- The SQL's rules carry over: `INSERT OR IGNORE` on links and relations,
  a taken entity id refused, a rename that keeps the row and re-keys it,
  `UPDATE OR IGNORE` then `DELETE` when links are repointed, the local
  `created_at` kept on a synced overwrite, and ASCII-only `lower()` for
  facts naming an entity.
- The graph's remaining SQLite callers move with it: the search entity
  scope reads links from the core, deleting a memory unlinks it through
  the new `Entities::unlink_memory`, and the unannotated batch becomes
  `Memories::unannotated_page`.
- Ties the SQL left unspecified now break by id on both backends: the
  mention-ranked entity page, linked memories, facts, relations, the
  unannotated batch. On the engine, `Entities::all`'s "storage order" is
  id order.
- A differential test runs every `Entities` method with sync on, on both
  backends, including repoints, renames, deletes, synced overwrites,
  unlinking and the unannotated batch. It compares every read and every
  queued and backfilled outbox payload.

**Core 3b, done: associations and the sync feed on the core.**

- The core gains `memory_associations`, keyed by its ordered pair.
  `Related` dispatches to it when the core is present: bumping a pair
  (a new pair at 1, a known one gaining 1 up to the cap), the relatives
  through shared entities, the document window, and co-retrieval.
- The engine reproduces the SQL's row multiplicity where it matters:
  `via_entities` yields a row per (seed link, neighbour link) pair, and
  co-retrieval reads a pair from each side it touches, so expansion's
  own grouping sees what it saw before.
- `SyncFeed` serves all four pull feeds and the graph counts from the
  core: memories and entities keyed on `(updated_at, id)` with the
  `node_id` exclusion, links on `(created_at, memory_id|entity_id)`,
  relations on `(created_at, id)`, each as the same wire record.
- Deleting a memory removes its pairs through the new
  `Related::unlink_memory`.
- The document window and co-retrieval now break ties by id on both
  backends.
- A differential test runs associations, all three expansion reads and
  every feed page (with cursors, limits and node exclusion) on both
  backends and requires identical answers.

**Core 3c, done: curation and promotions on the core. Core PR 3 is
complete.**

- The core gains `promotions`, keyed by its (promoted, source) pair and
  indexed by source. `Promotions` dispatches every statement to the core
  when it is present: the provenance writes and reads, the undecomposed
  dialogs, the entity fact groups, the ready scenarios and the persona
  statements.
- `Curation` does the same for the capture, normalization, maintenance
  and contradiction queues. The contradiction pairs keep the SQL's rules:
  distinct pairs `id_a < id_b` sharing an entity whose mention count
  (every link, live memory or not) is within the fan-out ceiling, both
  live and not dialogs, minus same-claim pairs compared with SQLite's
  `lower(trim())` (spaces only, ASCII only), keyset-paged.
- `normalized_from` matches only as text, as `json_extract(…) = m.id`
  does. A missing contradiction side is `StoreError::NotFound`, as the
  SQL's `query_row` makes it.
- Orders the SQL left unspecified are now fixed on both backends: a
  capture's rows by category then id, a capture's tags from its lowest
  id, the undecomposed and unnormalized queues newest first then id
  descending, the fact groups by size then entity id with each group's
  ids sorted, scenarios and statements by vitality or time then id, and
  the provenance lists sorted.
- A differential test runs every `Curation` and `Promotions` method over
  captures, imports, graph links, supersession and promotions on both
  backends and requires identical answers.

With core PR 3, the graph and everything that reads it are on the core.
What still reads SQLite beside memories: vectors and the import
bookkeeping (core PR 4a, below).

**Core 4a, done: vectors and the import bookkeeping on the core.** Core
PR 4 ships in two parts: 4a (this), and 4b, which makes the importers'
transactions journal batches.

- The core gains `vec_chunks` (keyed by the (memory, chunk index) pair,
  indexed by memory), `embedding_meta` (keyed by key), `chat_imports`
  (keyed by import id, indexed by hash), `dbs_imports` (keyed by the
  (source, external id) pair, indexed by memory) and `mempalace_imports`
  (keyed by drawer, indexed by memory). `Vectors` and `ImportLedger`
  dispatch every statement to them when the core is present, and the
  chat-import count in `StoreStats` moves with them.
- The SQL's rules carry over: `INSERT OR REPLACE` on chunks, the upserts
  on `embedding_meta` and `dbs_imports`, `INSERT OR IGNORE` on drawers, a
  plain `INSERT` that refuses a chat import id already recorded, and the
  joins that count only live memories. `LIKE` keeps SQLite's rules: `%`
  and `_` stay wildcards inside a caller's prefix, and case folds for
  ASCII only. `json_extract` on the drawer id reads a number or boolean
  as its text, and fails on malformed JSON.
- Orders the SQL left unspecified are now fixed on both backends: chunks
  in key order (so "any" embedding is the first by key), consolidation
  candidates oldest first then id (this also decides which ones a limit
  keeps), `embedding_meta` by key, the earliest chat import for a hash,
  and every id list the ledger returns sorted.
- A differential test on each repository runs every method on both
  backends, including replaced chunks, deleted, superseded and archived
  memories, orphan chunks, duplicate ids in the arguments and
  case-varied prefixes, and requires identical answers.

**Core 4b, done: an importer page is one journal batch. Core PR 4 is
complete.**

- `Store::transaction(work)` runs `work` as one transaction: a SQLite
  transaction, and when the core is present, one *page* on it. An error
  or panic in `work` rolls both back. The dbs and mempalace importers use
  it for each page in place of their own `unchecked_transaction`.
- A page's writes reach the stores as they are made, because the
  importer reads its own writes (an entity upserted two items earlier).
  An undo log (`node.undo`, a second journal) makes that safe. Before
  each write, the records it replaces go to the undo log, synced.
  Finishing the page commits all its writes as one redo batch, then
  empties the undo log, then checkpoints. Abandoning it puts the replaced
  records back, newest first.
- At open, the tables first undo whatever the undo log holds, then
  replay the redo journal. A crash before the redo commit leaves no trace
  of the page; a crash after it leaves all of it. Sequence values a page
  allocates become durable with its redo batch.
- While a page is open, only the thread that opened it may write to the
  core. Another thread's write is refused, not folded into a page it did
  not ask for. A write that fails part-way leaves the page able only to
  be abandoned. Another thread's reads see the page's writes before it
  finishes; SQLite's WAL hid them. The switch-on must decide how
  background writers wait for a page, since today their writes would be
  refused.
- Tests: finishing, abandoning, dropping an unfinished page, a crash
  inside a page, a crash between the redo commit and the undo
  checkpoint, and another thread's write during a page. A differential
  test runs a failed transaction and a successful one on both backends.
  Another runs a dbs import and a rerun that supersedes an item on both,
  with items that share a tag entity within one page.

With core PR 4, every table the memories core replaces is on it.

**Core 5a, done: the tests stop reaching around the repositories.**
Switching the core on locally broke about 330 tests in 52 suites, almost
all because they seeded or read memories and the tables that join them
with SQL, which the core never sees. So the switch-on is two PRs: this
one, then the flip.

- `remind_me_core::testing` (public, hidden from the docs) gives tests
  raw access that works on both backends:
  - one memory column, read or overwritten as an `UPDATE` would, with no
    outbox entry and no revision;
  - memory ids and row counts;
  - raw outbox entries and send markers;
  - feedback queries and association weights;
  - relabelling a memory's id.

  A differential test runs every helper on both backends.
- Every other seed and read goes through the repositories. Tests of what
  only SQLite has (its schema, its migrations, its triggers in an on-disk
  file) open SQLite explicitly with `Database::open_sqlite_in_memory`,
  now public.
- One production fix: an in-memory database on the engine now reconciles
  the `sync_enabled` gate after attaching the engine tables, as every
  open already did for SQLite's flags. Without it, the core's own
  `sync_flags` never had the gate set, and with sync configured nothing
  was queued.
- No test or assertion was removed.

**Next:** core PR 5b, the flip. The copy tool (§5) comes after it.

## Related

- ADR-0021 (the hub's move) and ADR-0022 (the seam this continues).
- ARCHITECTURE Tenet 3, retired in phase 0.
- The monorepo's `docs/adr/0003-workspace-layout-by-layer.md`, which
  decides where engine code lives.
