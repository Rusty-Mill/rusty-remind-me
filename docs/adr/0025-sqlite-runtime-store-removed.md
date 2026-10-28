# ADR-0025: The SQLite runtime store is removed

Status: Accepted (2026-10-04); implemented
Date: 2026-10-04

## Context

ADR-0023 moved the node's storage onto the embedded `rusty_multimodal_db`
engine in phases. Phase 5 made the engine the default store (2026-09-28):
the first open copied `memory.db` onto the engine, and every later open used
the engine directory beside it. The SQLite store stayed behind as a second
backend, selected by `REMIND_ME_STORE=sqlite`, and as the reference the
engine was tested against: every repository under `db/` was an enum over
the two, every differential test ran its case on both and compared, and CI
ran the whole suite once more with the SQLite store selected.

The owner decided (2026-10-04) that the engine is the node's only store,
and that the context-capture work (the 2026-10-04 review) builds on the
engine alone: new tables, filters and repositories are engine-only, and
SQLite is touched only where an old `memory.db` still has to be opened and
copied. Carrying the SQLite arm for that work would mean writing every new
repository twice and testing it against a backend nobody runs.

What the SQLite store still did at that point:
- a second implementation of every repository (about 12,000 lines of SQL
  and glue under `db/`, plus `schema_tables.sql`, `schema_indexes.sql`,
  the reconciling migrations and the open-time schema checks);
- the oracle for the differential tests;
- `sqlite3 memory.db` as a way to inspect a node's data;
- backups, through SQLite's online backup API;
- the one-time copy onto the engine, which read the file through
  `rusqlite`.

## Decision

The SQLite runtime store is removed. The engine is unconditional: the
`engine-store` cargo feature, `REMIND_ME_STORE`, `Database::open_on_sqlite`,
`Database::open_on_engine`, `Store::sqlite()` and every SQLite arm under
`db/` are gone. `Database::open` always opens the engine directory beside
the configured path, and `Database::open_in_memory` always opens temporary
engine tables. Each repository under `db/` is a thin layer over the
`db/engine/<group>.rs` module that owns its records, keeping the names
callers use (`Memories`, `Entities`, `Outbox`, …), so the rest of the crate
changed little.

One module still links SQLite, read-only: `db/legacy_sqlite.rs`. It does
three things and nothing else:
1. **The copy on first open.** A `memory.db` beside which no engine
   directory exists is opened read-only, every table copied onto the engine
   (`db/engine/copy.rs`, with its three rules: never write to the source,
   refuse rather than drop a row the engine cannot keep, verify every row
   after writing), and the file left as it was. It is never opened again.
   The copy reads a file at schema version 30, 31 or 32; v30 and v31 lack
   columns the engine record defaults, and v29 keyed its vectors on row
   numbers the copy has nowhere to put, so an older file has to be opened
   once with rusty-remind-me 0.2.x first. After a copy, entity ids an
   earlier build derived differently are rewritten once, as the SQLite
   store's open used to do on every open.
2. **The foreign-file importers.** `dbs_import` and `mempalace_import` read
   other programs' SQLite databases (ADR-0023 §6) through the same
   read-only reader. Their errors are `StoreError::Legacy` now, not
   `rusqlite::Error`.
3. **Test fixtures.** `legacy_sqlite::fixture` runs SQL against a file so a
   test can build what another test reads; it is the only write the module
   has. The copy's own tests read two checked-in files,
   `tests/fixtures/legacy_store/legacy_v32.db` and `legacy_v31.db`, written
   by the last build that stored in SQLite, with a row in every table.

`StoreError::Sqlite(rusqlite::Error)` became `StoreError::Legacy(String)`,
used only by that module: the name says what the error is about, and
holding the message rather than the error type keeps `rusqlite` out of the
crate's public API.

The CLI keeps `copy-store`, which copies an old file into a separate engine
directory without touching the node.

Backups are a copy of the engine directory, taken while the tables are held
so no write lands half-copied, into `backups/<label>-<timestamp>.engine`
beside the configured path, built in a `.partial` directory and renamed into
place. Restoring is putting the directory back as `memory.engine`. Backups
taken while the node stored in SQLite (`backups/*.db`) still list and are
pruned in turn. `BackupError::Sqlite` and `BackupError::NotSqlite` are gone.

The `Store` is no longer an enum: it carries the engine handle, the path
the store is configured at, and, when it came from `Database::store`, the
process-wide lock that orders tool calls as the SQLite connection's mutex
once did. A background thread's `SecondarySource::store()` holds no lock;
the engine tables lock per call. The thread-local SQLite connections those
threads used to open are gone with it, so a path-only `SecondarySource`
cannot exist any more: an in-memory database still starts no background
loop, now because it lives only as long as its `Database`.

Two things the SQLite open did on every open now happen on the engine's:
the outbox is pruned by the retention rule, and a changed embedding model
clears the stored vectors. In phase 5 both ran against the SQLite file,
which the engine store had stopped reading.

## Consequences

**Gains**
- One store, one implementation of every repository: about 12,500 lines
  of SQL and glue gone, and no second arm to write for the context-capture
  tables.
- The test suite runs once, on the engine. The differential tests became
  direct tests of the engine's behaviour, with the SQLite results they used
  to compare against written down as expectations.
- `rusqlite` is linked in one module, read-only, and the hard rule is
  checkable: `grep -rn rusqlite crates/*/src | grep -v legacy_sqlite` is
  empty.

**Costs**
- `sqlite3 memory.db` no longer shows the node's data. A `dump` command is
  the likely follow-up ADR-0023 already named.
- The SQLite file is an upgrade source, not a store: a node that still
  wants its data in SQLite has no path back. The hub's copy tool
  (`rusty-remind-me-hub-copy`) is the model for such a tool if one is ever
  wanted.
- A `memory.db` at the Python reference's v29 cannot be copied by this
  build. It has to pass through rusty-remind-me 0.2.x once.
- Two processes that both find a `memory.db` with no engine directory
  race to copy it. The SQLite store used to serialise them on the file's
  write lock; now the second fails on the `.partial` directory's lock and
  retries at its next start, by which time the first has finished.
- The hub's SQLite reader (`remind_me_hub/src/import/legacy_sqlite.rs`,
  renamed from `import/sqlite.rs`) is unchanged: it reads a retired hub
  store for the hub's copy tool (ADR-0021).

## Alternatives declined

- **Keep SQLite as a selectable store a while longer.** Every new
  repository for the context-capture work would have needed a SQLite arm,
  or a "not supported" error on that arm, and the differential tests would
  have had to keep an oracle nobody runs in production.
- **Keep the SQLite schema files for documentation.** They described a store
  that no longer exists and would have drifted from the engine records.
  `db::testing::MEMORY_COLUMNS` and each group's engine record are the
  schema now, and `legacy_sqlite::SCHEMA_VERSION` the version the records
  mirror.
- **Rewrite the differential tests as snapshot comparisons against the
  fixtures' read-back.** The snapshots would have been `Debug` output of
  the model structs, impossible to regenerate once the SQLite store was
  gone. The expectations are written as values instead.

## Related

- ADR-0021: the hub's move to the engine, whose copy rules this follows.
- ADR-0023: the node's move to the engine; this is its phase 6.
- ADR-0024: tombstones are emptied, not purged, on every open.
