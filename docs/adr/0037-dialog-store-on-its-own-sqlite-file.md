# ADR-0037: The dialog store is its own SQLite file

Status: Accepted (2026-11-25; D75 to D81, Nano "1618 is SQLite" and "Concur")
Date: 2026-11-25

Amends ADR-0025 for the dialog store and the artifact module only. Builds
on ADR-0035 (one port per table group).

## Context

Mill Board #528: verbatim dialogs are the source of truth and must never be
cleaned; everything else is a projection. The R3 store question (#1618)
was settled by Nano choosing SQLite (D75). That is the owner's choice, not a
gate result: the scoring run (draft #425) finds no candidate passing every
gate, SQLite included (it misses G4), and leaves the pick to him. The engine holds every row in RAM, which a 4 GB
host cannot afford for full dialog bodies.

## Decision

1. Dialogs live in `dialogs.sqlite3`, a separate file beside `memory.engine`,
   in the same binary (D76). `memories`, entities and the rest stay on the
   engine (D79).
2. The port `db::dialogs::DialogStore` has append and read only. The SQLite
   adapter also refuses UPDATE and DELETE with triggers, so the file itself
   is append-only. There is no delete path, including the owner's (D80).
3. A row holds metadata and a 512-byte head (D51). The body is a
   `BlobStore` blob named by its sha256 (D50). `DialogLog` publishes the
   blob first with `BlobStore::put_durable` (synced file; every directory
   it created synced, with the entry naming it in its existing parent; an
   existing blob verified and replaced if it fails its hash), then appends
   the row; a failed publication appends nothing, and a rerun is
   idempotent by hash. Durability boundary: the nearest ancestor that
   existed before the write is taken to be durable. On Unix a corrupt blob
   is replaced by one atomic rename, never unlinked first. Elsewhere there
   is no directory sync and the replacement removes before it renames, so a
   lost rename or a concurrent reader shows a missing blob on read, never
   wrong bytes.
4. `append` is one `INSERT ... ON CONFLICT DO NOTHING` followed by a read
   of the winner's row, so two connections racing on an id get
   `Duplicate` for identical content and `Invalid` for different content,
   and the original row is never changed.
5. ADR-0025's rule that `rusqlite` appears only in `legacy_sqlite` becomes:
   only in `legacy_sqlite` and `db::sqlite`.

## Not in this change

Wiring captures to the log, excluding dialogs from `memories`, search over
the dialog store (D81), the backup package (D78), the reconcile on open, the
rebuild check (D77) and the migration of existing dialog rows. The
migration is a data migration (D3): nothing runs on the live store without
Nano's approval of a dry run.
