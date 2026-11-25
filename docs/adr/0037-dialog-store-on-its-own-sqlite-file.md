# ADR-0037: The dialog store is its own SQLite file

Status: Accepted (2026-11-25; D75 to D81, Nano "1618 is SQLite" and "Concur")
Date: 2026-11-25

Amends ADR-0025 for the dialog store and the artifact module only. Builds
on ADR-0035 (one port per table group).

## Context

Mill Board #528: verbatim dialogs are the source of truth and must never be
cleaned; everything else is a projection. The R3 store benchmark (#1618)
came out SQLite (D75), and the engine holds every row in RAM, which a 4 GB
host cannot afford for full dialog bodies.

## Decision

1. Dialogs live in `dialogs.sqlite3`, a separate file beside `memory.engine`,
   in the same binary (D76). `memories`, entities and the rest stay on the
   engine (D79).
2. The port `db::dialogs::DialogStore` has append and read only. The SQLite
   adapter also refuses UPDATE and DELETE with triggers, so the file itself
   is append-only. There is no delete path, including the owner's (D80).
3. A row holds metadata and a 512-byte head (D51). The body is a
   `BlobStore` blob named by its sha256 (D50). `DialogLog` writes the blob
   first, then the row; a rerun is idempotent by hash.
4. ADR-0025's rule that `rusqlite` appears only in `legacy_sqlite` becomes:
   only in `legacy_sqlite` and `db::sqlite`.

## Not in this change

Wiring captures to the log, excluding dialogs from `memories`, search over
the dialog store (D81), the backup package (D78), the reconcile on open, the
rebuild check (D77) and the migration of existing dialog rows. The
migration is a data migration (D3): nothing runs on the live store without
Nano's approval of a dry run.
