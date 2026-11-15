# Audit: can DBS fold into remind-me as a module with its own storage?

Status: Direction chosen. Nano picked option A, "Refactor first, store at benchmark", on 2026-11-15. Design only: no code, schema, data or API change. Each build step still needs his go-ahead.
Date: 2026-11-15. Read at rusty-remind-me `9aff941`, rusty_mill `7c5bc795`,
Daily-Backup-System `6cc6491`, rusty_dbs `fd2bf76`.

## The answer in brief

- **remind-me's live store is not SQLite.** It has been the embedded `rusty_multimodal_db_engine` since
  2026-09-28, and the only store since 2026-10-04 (ADR-0023, ADR-0025). SQLite is linked only to read
  an old `memory.db` once, and to read other programs' files (`dbs_import`, `mempalace_import`). The
  2026-11-06 voice note ("the node may still be on SQLite") is stale. The `memory.db` path in
  `server_status` is the name the engine directory sits beside (`memory.engine`).
- **DBS is SQLite.** That holds for both the Python Daily-Backup-System and the Rust port `rusty_dbs`.
  Both use one file with FTS5, revisions, and media blobs stored in the database.
- **remind-me's storage sits behind a seam, not a port.** It has 20 concrete repositories over one
  engine handle (ADR-0022), and there is no trait. The domain is well insulated from the engine: no
  production code outside `db/` touches it except the CLI's deliberate `copy-store`. The coupling sits elsewhere. All tables live in one closed
  `EngineTables` struct with one journal and one lock, and search and the ladder only understand the
  `memories` table.
- **A DBS module with its own store can plug in without touching the memory schema,** if two rules
  hold. Raw artifacts live outside `memories`, with their bodies as blobs on disk and not in engine
  rows. What the ladder distils from them becomes ordinary memories that link back to the artifact.
  #528's append-only dialog store is the same shape, so it should use the same module.
- **Recommendation: refactor first, and fold in DBS's knowledge rung, not its backup machinery.** Cut
  four small seams, all independent of which store wins. Then decide the store at the R3 benchmark,
  not before it. Do not move DBS or remind-me's raw artifacts onto `rusty_multimodal_db_engine` as it
  stands: it holds every record in RAM, and the remindme host has 4 GB.

## 1. Storage today, verified from code

| System | Store | Evidence |
|---|---|---|
| remind-me node | `rusty_multimodal_db_engine`, directory `memory.engine` beside `memory.db` | `crates/remind_me_core/src/db/mod.rs:249` (`Database`), `db/engine/mod.rs:136` (`EngineTables`); ADR-0025 |
| remind-me, SQLite | Read-only: the one-time copy of an old `memory.db`, the `dbs`/mempalace importers, and test fixtures | `db/legacy_sqlite.rs` is the only `rusqlite` user in the crate |
| Daily-Backup-System (Python) | SQLite: `sources`, `sync_runs`, `items`, `item_revisions`, `media` (with a `data BLOB`), `sync_state`, `source_locks`, plus `items_fts` (FTS5) | `src/dbs/storage/migrations.py`, `sqlite.py:99`; port `storage/base.py:82` |
| rusty_dbs (Rust port of the above) | The same schema on `rusqlite` 0.32 | `crates/dbs-core/src/storage/{migrations,sqlite_storage}.rs` |

Which DBS runs in production (Python or Rust) is not recorded in the repos. Both read and write the
same schema, so the answers below hold for either.

## 2. remind-me's crates and modules

Five crates, 81,905 lines of Rust, and `remind_me_core` holds 82% of them.

```mermaid
flowchart TD
  cli[remind_me_cli<br/>2.3k: binary, daemon, session, copy-store]
  remote[remind_me_remote<br/>1.0k: HTTP MCP, OAuth]
  mcp[remind_me_mcp<br/>9.0k: tool surface, render, daemon proxy]
  api[remind_me_api<br/>2.8k: dashboard routes + JSX]
  subgraph core[remind_me_core 66.8k]
    domain[domain: capture/decompose, promotion ladder,<br/>retrieval+RRF, vitality, entities, wiki, review,<br/>importers incl. dbs_import, archive, backup]
    repos[db/*: 20 concrete repositories<br/>Memories, Entities, Archives, Wiki, ...]
    eng[db/engine/*: EngineTables<br/>one dir, one journal, one DirLock]
    legacy[db/legacy_sqlite: read-only rusqlite]
    domain --> repos --> eng
    domain -. dbs_import, mempalace .-> legacy
  end
  cli --> mcp & api & remote & core
  remote --> mcp --> core
  api --> core
  eng --> mmdb[(rusty_multimodal_db_engine<br/>libs-2026.10.0)]
  legacy --> dbsfile[(DBS SQLite file)]
  cli -. copy_store.rs:48 .-> eng
```

Outside core, the boundaries are clean. `remind_me_remote` never touches storage. The MCP layer
holds an `Arc<Database>` and imports only `db::references`. The API layer imports `db::queries` and
`db::stats`. The CLI's `copy-store` command calls `db::engine::copy` on purpose.

## 3. Is storage behind a port?

No. It sits behind a seam, as ADR-0022 decided: build the seam, and extract traits only when a
second backend is real. In practice that means:

- Domain modules take `&Store<'_>` (98 files) and call narrow repositories that speak domain types.
  No domain module imports the engine crate, and the SQL that used to be inline has gone.
- `Store` is a concrete engine handle, not an abstraction. Tests run on in-memory engine tables, not
  on fakes.

The leaks (dashed lines in the diagram):

| Leak | Where | Size |
|---|---|---|
| ~~A domain module builds an engine row~~ Corrected 2026-11-15: `review.rs:427` is inside `#[cfg(test)]`, a test fixture, so it is not a leak | `remind_me_core/src/review.rs:427` | None |
| The CLI drives the engine copy directly | `remind_me_cli/src/copy_store.rs:48` | Intentional (an ops tool); leave it |
| **One closed table set.** Every table group is a field of `EngineTables`, under one journal, one undo log, one page and one `DirLock` | `db/engine/mod.rs:136-200`, `db/engine/core.rs:47` | The real coupling: a module cannot bring its own tables without editing this struct |
| **Search knows one table.** Full-text is a `FullTextIndex` over `memories` built at open, and vectors are keyed by `memory_id` | `db/engine/memories.rs:47`, `db/engine/core.rs:70`, `db/engine/vectors.rs:36` | By design; see §4 |
| **"Raw" is a category, not a store.** Dialogs are rows in `memories` with category `dialog`, special-cased in 12 source files | `models.rs:967` `DIALOG_CATEGORY`; `capture.rs`, `context_brief.rs`, `session_ops.rs`, `transcript.rs`, `db/curation.rs`, `db/engine/promotions.rs`, `remind_me_mcp/src/lib.rs`, … | The coupling #528 has to undo |

The removal of the hub and sync (ADR-0032) supports this reading. A sixth crate and eleven core
modules came out with no domain rewrite, which shows the module seams are real. Storage is the
exception: sync's outbox still lives inside `CoreTables` (phase 2 of ADR-0032 is still open).

## 4. Could a DBS module plug in without touching the memory schema?

Yes, in this shape:

```mermaid
flowchart LR
  subgraph dbs[DBS collectors, unchanged]
    conn[14 connectors<br/>subprocess binaries] --> dbsdb[(DBS SQLite)]
  end
  clip[clip pipeline #763<br/>markdown + frontmatter]
  dbsdb --> ingest
  clip --> ingest
  subgraph rm[remind-me]
    ingest[artifact ingest port<br/>idempotent by content hash] --> art[(artifact store<br/>own dir: rows = metadata,<br/>bodies = content-addressed blobs)]
    art --> ladder[existing ladder<br/>decompose, promote, wiki compile]
    ladder --> mem[(memories<br/>unchanged schema)]
    mem --> search[existing search + brief]
    mem -. metadata.source_artifact .-> art
  end
```

- **Its own store.** A second table set in its own directory (for example `artifacts.engine`), with
  its own journal and lock. Each row holds only metadata: source, source id, url, dates, tags, fetch
  path, content hash and blob path. Bodies are blobs on disk, as `archive.rs` already stores raw
  transcripts (content-addressed under `<root>/<hash[0..2]>/<hash>`).
- **Its own ingest.** A port that takes an artifact and writes it once per content hash. The first
  adapter reads DBS's SQLite read-only, as `dbs_import.rs:339` (`pull_dbs`) already does. The second
  reads the #763 clipped markdown.
- **The shared ladder and search.** Distilled summaries and facts are ordinary memories carrying
  `metadata.source_artifact` (store, id, hash). Search, the brief, the wiki and the vitality model
  work unchanged, and "show me the original" follows the link, as `remind_me_get raw=true` does for
  archive spans today. Searching raw artifacts directly is not needed in v1. ADR-0029 already ranks
  refined memories first and raw dialogs only on request.
- **No cross-store transaction.** Two directories mean two journals. Ingest therefore writes the
  artifact first and the derived memories after, and a rerun is idempotent by hash. A memory that
  points at a missing artifact is the one inconsistency, and a reconcile on open can list it. That
  replaces the atomic write rather than emulating one.

**#528's dialog store is the same module.** A dialog is an artifact of kind `dialog`, and append-only
means the port has no update or delete. Its decay exemption is implicit, because artifacts are not
memories. It gets a separate backup because the directory is separate. Rebuild means replaying
artifacts through decompose. The one difference is that dialogs already exist inside `memories`, so
moving them out is a data migration (seam 7 below). DBS items have no such history.

**What should not fold in:** DBS's backup machinery (connectors, revisions, soft-delete sweeps,
locks, scheduler, web UI). Its connectors are already subprocess binaries with Playwright and yt-dlp
dependencies (rusty_dbs ADR-0001), and that is a real fault-isolation boundary. remind-me needs DBS's
output, not its job runner.

## 5. Seams to cut, ranked by effort

| # | Seam | Effort | Files | Store-neutral? |
|---|---|---|---|---|
| 1 | Provenance contract: `metadata.source_artifact {store, id, hash}` on memories built from an artifact. **Done in this PR** for the first writer, `dbs_import` (`source_artifact.rs`). Decompose and promotion adopt it with the artifact module | S | `source_artifact.rs`, `dbs_import.rs`; later `capture.rs:310`, `promotion.rs:395` | Yes |
| 2 | ~~Close the domain leak in `review.rs`~~ Dropped: the reference is test-only (see §3) | None | | |
| 3 | Lift `archive.rs`'s content-addressed blob store into a reusable blob store with its own root, included in backups. **Deferred to seam 5:** its only user today is `archive.rs`, so extracting it now is an abstraction with one caller | S | `archive.rs:72-180`, `backup.rs` | Yes |
| 4 | A second engine directory: extract open, journal, `DirLock`, compaction and backup from `EngineTables` so a module opens its own table set beside `memory.engine` | M | `db/engine/mod.rs:136-260`, `db/mod.rs:249-331`, `compaction.rs`, `backup.rs` | Engine-specific; under SQLite this is one more file |
| 5 | Artifact module: ingest port, artifact repository, and the DBS and clip adapters. `dbs_import` moves from writing memories directly to writing artifacts and letting the ladder distil them | M | new `artifacts/`; `dbs_import.rs:339` | Yes, behind the port |
| 6 | The R2 storage port: traits over the existing narrow repositories, so R3 can run the same tests and golden set on the engine, SQLite and Ledger's store | L (broad but mechanical: about 98 caller files, about 757 test setups) | `db/*.rs` | That is its purpose |
| 7 | #528: move existing dialogs out of `memories` into the artifact store. A data migration: dry-run count, crash-safe upgrade (the `legacy_upgrade` pattern from PR #368), rollback. Remove the `DIALOG_CATEGORY` special cases from 12 source files | L | `models.rs:967` and the files in §3 | Needs seams 3 to 5; needs Nano |

Seams 1 and 3 are worth doing on any path; seam 1 is done, and seam 3 lands with its second user. Seam 4 is needed only if the engine stays. Seam 6 is
Millwright R2.

## 6. DBS on `rusty_multimodal_db_engine`: readiness

What a move would need:

- Reimplement `SqliteStorage`: 3,083 lines and a 32-method `Storage` trait (`dbs-core/src/storage/mod.rs:169`).
  The work covers upsert with classification, revision history, soft-delete sweeps, browse filters,
  FTS5 search, metrics, `integrity_check`, `prune_revisions` and `VACUUM INTO`. The engine has no
  query language, so each filter becomes hand-written index code.
- Fix the port first. `ItemRow` is `HashMap<String, Value>` (`storage/mod.rs:97`), which makes it a
  loose dict and not a type. `dbs-web/src/api.rs:87` opens `SqliteStorage` directly, and the 2026-11-13
  Python audit found that port has drifted from its only adapter.

Gaps and risks in the engine, from its README and code:

| Risk | Detail | Weight for DBS / raw artifacts |
|---|---|---|
| **All records in RAM** | Each store keeps every record in a `HashMap`, and the full-text index is in memory, rebuilt at open | **Blocking.** About 50k Raindrop pages at an estimated 10-30 KB of markdown each is roughly 0.5-1.5 GB of text before indexes. That is an estimate, not a measurement. The host has 4 GB and already OOMs on builds |
| One process per directory, one writer | `DirLock`; a single `RwLock` that panics if poisoned | Acceptable for one server; rules out DBS's web UI and a backup run writing at once unless they share a process |
| Insert log grows until compaction | remind-me compacts on a timer (`compaction.rs`) | Manageable |
| Backups copy the directory under the lock | `backup.rs` | Fine at MB, stop-the-world at GB |
| Positional (bincode) records | A new field means a schema-tag bump and an upgrade module | Every DBS schema change becomes a migration |
| Crash safety | Redo journal, fsync'd insert log, undo log for pages, with tests in `journal_batches.rs` and `store_lifecycle.rs` | I found no power-loss or fault-injection test. Not proven to be absent, but unproven |
| Query features | Exact FTS5 parity (`fulltext_vs_fts5.rs`), plus `Indexed`, `Ordered` and `Scanned` layers. No ad hoc filters, joins or SQL | DBS's browse and export filters must be rewritten |

Verdict: the engine is ready for remind-me's memory-sized data, which is what it runs today
(24,067 memories). It is not ready to hold raw artifacts or DBS archives until storage is paged,
which is the P1 "paged storage" item in its own 2026-11-01 review and ADR-0138. That hardening is a
card in the rusty_multimodal_db project, not here.

## 7. Recommendation and options

| Option | What happens | For | Against |
|---|---|---|---|
| **A. Refactor first, store decided by benchmark (recommended)** | Cut seams 1 to 3 now (S, store-neutral). Build the artifact module (seams 4/5) on whatever store R3 picks. DBS collectors stay separate and feed the ingest port. #528 follows on the same module | Nothing done twice. Raw bodies stay off the RAM-resident tables whichever store wins. Matches Nano's 11-10 preferences (an off-the-shelf store and C code are acceptable) and the Millwright draft (R2 port, R3 benchmark, R4 intake) | Knowledge intake waits for R3 and R4. #763's clipping can still run ahead into its raw folder, as already agreed |
| B. 11-06 sequence: harden multimodal_db, migrate, fold in | Paged storage in the engine first (its own project), then DBS onto the engine, then fold in | One database across Nano's apps, which is the 11-06 goal | Blocks on engine work that has no date. Rewrites 3k lines of working SQLite code. Commits to the engine before it has been measured |
| C. Keep DBS separate | Leave it as it is. The existing read-only `dbs_import` keeps pulling items in as memories | Works today, zero work | Raw text lands in `memories` (bloat, decay, RAM). No append-only originals. Two stores and two backups with no provenance contract |
| D. Fold in as-is | Put DBS's tables into `EngineTables` | none | Fails on RAM, the closed struct and positional rows. Not viable |

The direction conflict to settle: the card's agreed sequence (Nano, 2026-11-06: harden
multimodal_db, migrate, then fold in DBS) and the Millwright draft order (QUESTIONS.md R1, R3, R6:
port, then benchmark, then migrate only if the winner changed) are both on record. Neither
supersedes the other yet (Millwright handoff Q12). Option A is Millwright's order, and option B is
11-06's.
