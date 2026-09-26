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
| 1 | **Finish the seam.** Put every remaining table group behind a repository, then move the triggers and echo suppression into the repositories (FTS, tags, outbox). Re-key vectors on memory id. Drop `&Connection` from callers. | Several PRs, one group at a time, SQLite still underneath |
| 2 | **The daemon.** `rusty-remind-me daemon` on SQLite; MCP, CLI, hooks, `api` and `remote` become clients; auto-start. | Behaviour-preserving |
| 3 | **Engine additions.** Full-text index, write batches, sequence allocators, directory lock (§3). | Engine PRs, each with its own tests |
| 4 | **The engine-backed store.** `NodeStore` on the engine, run by the same test suite as the SQLite store, plus the copy tool (§5). | Behind a feature, off by default |
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
| 4 | Vectors, re-keyed on memory id (§4) | `db::vectors` | |
| 5 | Promotions, imports, archives and curation queues | `db::promotions::Promotions`, … | In progress |
| 6 | The outbox, with echo suppression as a flag | `db::outbox::Outbox` | Statements moved; the flag waits for step 7 |
| 7 | Triggers move into the repositories | | |
| 8 | Callers stop taking `&Connection` | | |

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

## Related

- ADR-0021 (the hub's move) and ADR-0022 (the seam this continues).
- ARCHITECTURE Tenet 3, retired in phase 0.
- The monorepo's `docs/adr/0003-workspace-layout-by-layer.md`, which
  decides where engine code lives.
