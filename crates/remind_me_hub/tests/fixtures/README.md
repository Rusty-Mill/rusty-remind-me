# Recorded hubs and answers

The hub used to store its data in SQLite and Postgres as well as the embedded
engine, and a differential test held the three to one another. Those stores
were removed (ADR-0021, phase 3). Before they went, their answers and the
databases they wrote were recorded here, from `main` at `72299199d`, so the
engine and the copy tool are still held to exactly what they did.

These files are the reference now. Nothing regenerates them: the stores that
produced them no longer exist. Change one only when the hub's intended
behaviour changes, and say why in the commit.

| File | What it is |
| --- | --- |
| `script_answers.json` | The SQLite store's answers to the script in `../suite/recorded.rs`: whether each push applied, every read after the pushes, and every read after compacting tombstones at `COMPACT_CUTOFF`. The engine agreed with it exactly, and Postgres agreed up to `hub_seq` gaps. |
| `sqlite_hub.sql` | The SQLite hub database after that script, before compaction, as SQL. |
| `sqlite_hub_compacted.sql` | A SQLite hub whose newest memory was compacted away: `hub_meta` holds 2, the one remaining row holds 1. |
| `sqlite_hub_invalid.sql` | A SQLite hub holding a memory with a 65-byte id and a link to it, which the engine cannot store. |
| `postgres_hub.sql` | The Postgres hub after the script and one more LWW loss, from `pg_dump --inserts` (psql meta-commands and comments stripped). Every loss spent a `nextval()`, so its `hub_seq`s have gaps and its sequence ends above every row. |
| `postgres_answers.json` | The Postgres store's answers to every read of that hub, and the `hub_seq` it gave the next write. |
| `legacy_postgres_hub.sql` | The Python hub's schema (`TIMESTAMPTZ` timestamps, 11 columns) with two rows. |
| `legacy_postgres_migrated.json` | Every memory the Postgres store served after migrating that database in place. |

The SQLite dumps were written by walking `sqlite_master` and quoting each
row with SQLite's `quote()`. A test rebuilds each database from its dump, so
the rows are the store's own, though the file's page layout may differ.
