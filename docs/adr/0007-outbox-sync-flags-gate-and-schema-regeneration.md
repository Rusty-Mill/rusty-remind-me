# ADR-0007: Regenerate the schema dump; gate every outbox trigger on `sync_flags`

Status: Accepted
Date: 2026-07-30

## Context

Filed as `#76` while implementing `#57`'s memories-only slice (PR #75): the
installed `schema_triggers.sql` was dumped from a `remind_me` snapshot that
predates the trigger shape the reference actually ships today. Two things
were confirmed by directly re-running the reference's real schema code
(`remind_me_mcp/db.py`'s `_ensure_schema`/`_migrate_schema`, imported
standalone against a fresh in-memory SQLite connection, with only its
optional `embeddings`/`ann_index` dependencies stubbed out — not a
hand-transcription, the actual generated DDL) against a fresh dump:

1. **`memories_outbox_ai`/`_au` were missing a `WHEN` guard** and three
   payload columns (`doc_id`, `chunk_index`, `deleted_at`) the reference's
   `_migrate_v15_to_v16` added. Without `deleted_at` on the wire, a
   soft-deleted memory (`#57`'s `delete_memory` tombstone path) had no way to
   propagate its deletion to another node at all.
2. **`entities_outbox_ai`/`_au`, `entity_relations_outbox_ai`, and
   `memory_entities_outbox_ai` are themselves part of the reference's
   generated schema** (added with the entity graph in `_migrate_v9_to_v10`/
   `_migrate_v13_to_v14`), not something this crate needed to invent. An
   earlier pass of this port (`#57`'s graph-sync slice) installed hand-rolled
   equivalents because the schema dump it started from predated them.

Both gaps trace back to the same root cause: every one of these triggers is
gated in the reference on `COALESCE((SELECT value FROM sync_flags WHERE key
= 'sync_enabled'), '0') = '1'`, reconciled against the live configuration on
every startup by `_reconcile_sync_enabled_flag` (`db.py`). This crate's
outbox triggers had no gate at all, so they fired unconditionally on every
write regardless of whether sync was ever configured — the reason `#59`'s
`prune_outbox` had to exist in the first place, working around a growth
problem the reference's own gate prevents at the source.

## Decision

**Regenerated `schema_tables.sql`/`schema_indexes.sql`/`schema_triggers.sql`**
from the reference's real schema code rather than hand-editing the confirmed
columns in. Concretely: `remind_me_mcp.db`'s `_ensure_schema` was imported
directly (stubbing only `embeddings`, which needs `numpy` purely to import,
irrelevant to DDL) against an in-memory `sqlite3.Connection`, and the
resulting `sqlite_master` dump — `IF NOT EXISTS` re-added and FTS5's own
shadow tables/`sqlite_sequence` excluded, matching this repo's existing
convention exactly — replaced the stale trigger/index definitions. This
stays true to "generated verbatim, not hand-written" (`migrations.rs`'s own
documented contract) without needing a real `remind_me` server process
running (which `#76`'s own investigation found blocked on an unrelated MCP
SDK version mismatch in this environment).

Net schema diff: `idx_memories_source_capture_id` (a missing index),
`memories_outbox_ai`/`_au` gained the `WHEN` gate and three payload columns,
and the four graph-table outbox triggers moved from `sync::graph`'s
hand-rolled `ensure_schema` into the generated file itself (removing that
function and its `OWN_ADDITIONS` allowlist entry in `schema_test.rs`
entirely — they are no longer a second, diverging source of truth).

**Added `sync::reconcile_sync_enabled_flag`**, called on every
`initialize_schema` (matching the reference's every-startup call site),
implementing `_reconcile_sync_enabled_flag`'s exact stored/desired matrix:

- already matches: no-op.
- stored `"0"`, now enabled: backfill `sync_outbox` with an `insert` row for
  every current `memories`/`entities`/`memory_entities` row (**not**
  `entity_relations`, matching the reference's own omission exactly rather
  than silently covering more than a first sync from the reference would).
- now disabled (from any prior stored state): `sync_outbox`/`sync_sends` are
  cleared.
- unset (a fresh database) and now enabled: **no backfill** — reproducing the
  reference's exact stored/desired matrix even though its own stated
  rationale ("pre-gate triggers were unconditional, so the outbox is already
  complete") describes reference history this crate never had. One
  reconciliation function matching the matrix beats two diverging ones for
  "true fresh" vs. "upgraded from an older, once-ungated build."

**`SyncRecord` gained `deleted_at`**, applied through the exact same LWW path
as every other column (no special-casing) — this is what actually lets a
tombstone propagate. Deliberately did **not** add `doc_id`/`chunk_index`:
the reference's own receiving side (`sync.py`'s `_upsert_one`) sends them on
the wire for column-list parity but never reads them back off an incoming
record either, so there is nothing for this port to apply.

## Alternatives considered

**Hand-editing the three confirmed columns and the `WHEN` clause into the
existing `schema_triggers.sql` without a real re-dump.** Rejected: this file
is documented as generated verbatim, and the earlier hand-transcribed
migration ladder (referenced in `migrations.rs`'s own history) is the exact
class of mistake this crate moved away from. Running the reference's actual
schema code, even standalone, stays on the right side of that line.

**Skipping the `entities_outbox_*` moves and leaving `sync::graph`'s
hand-rolled triggers in place, just adding the gate to them separately.**
Rejected: once the fresh dump showed the reference ships these triggers
itself, keeping a hand-rolled copy would mean this crate maintains its own,
separately-evolving definition of something that already has a canonical
source — exactly the divergence risk the generated-schema convention exists
to prevent.

**Embedding `#76`'s tombstone-cleanup step (removing a remotely-tombstoned
memory's chunk vectors)** now. Deferred: that step depends on `#49`'s vector
storage, which isn't part of this branch. Tracked as a follow-up once `#49`
lands, not invented ahead of it.

## Consequences

- A node that has never configured sync now queues nothing in
  `sync_outbox` at all, for any table — the growth problem `#59`'s
  `prune_outbox` works around no longer occurs at the source for a node that
  never turns sync on, matching the reference exactly.
- A soft-deleted memory's tombstone now reaches every synced node, closing
  the concrete gap `#76` was filed over.
- Every test that asserted a local write reaches `sync_outbox` without
  configuring sync had to start configuring it first — a mechanical but
  wide-reaching test update (`outbox_test.rs`, `sync_test.rs`,
  `graph_sync_test.rs`, `schema_test.rs`), since the whole point of the gate
  is that an unconfigured node's writes must not reach the outbox at all.
- A periodic hard-delete compaction pass for old tombstones
  (`sync._compact_tombstones` in the reference) is not implemented here —
  noted as a real, separate gap for a follow-up issue, not silently rolled
  into this one.

## Addendum, 2026-08-03: the gate needed a second condition, and triggers needed reconciling (issue #100)

The `sync_flags` gate above stops an *unconfigured* node queueing anything.
It does nothing for a configured one, and on a configured node
`memories_outbox_au` fired on every `UPDATE` — including the `accessed_at`/
`access_count` write this crate makes on every read (PR #42, `vitality.rs`).
So a 20-result search queued 20 full-payload rows. The reference scoped this
out in `_migrate_v21_to_v22` with a second `WHEN` condition,
`AND NEW.updated_at IS NOT OLD.updated_at`; this crate's schema, dumped from a
*v19* reference database, predates it.

Two decisions follow.

**Forward-ported the guard by hand rather than waiting for the v27
regeneration.** This is the thing "Alternatives considered" above rejected, so
it needs justifying rather than glossing. Three things make it different from
the case that was rejected there:

1. The rejected alternative was hand-*reconstructing* DDL that nobody had
   observed — the same guesswork that produced the bad migration ladder. This
   line is copied from the reference's own source, verified against
   `db.py:1944`, and is one condition on an existing `WHEN`.
2. It is self-healing. Issue #101 regenerates all three files from a v27 dump,
   which contains this exact line — so the exception disappears by being
   overwritten with an identical value, not by anyone remembering to reapply
   it. `schema_triggers.sql`'s header says so at the point of the edit.
3. The alternative was not "wait a bit". #101 is a breaking change gated on a
   human decision, with no committed date; leaving every configured vault
   flooding its own outbox until then is a worse outcome than one annotated,
   self-erasing line.

Stamping v19 while carrying a v22 trigger is safe in the direction that
matters: `remind_me` opening such a database reads `user_version = 19` and
runs `_migrate_v21_to_v22`, which begins `DROP TRIGGER IF EXISTS
memories_outbox_au` and recreates it — so the reference overwrites our
forward-port with its own identical definition rather than tripping over it.

**Added trigger reconciliation to `migrations.rs`.** Fixing the DDL alone
would have shipped nothing to anyone who already had a database. Every
statement in `schema_triggers.sql` is `CREATE TRIGGER IF NOT EXISTS`, and
`apply`'s reconciliation loop compared `type='table'` rows only — so an
existing database kept its old trigger forever and the fix would only ever
have appeared on databases created after it. `reconcile_triggers` now diffs
each trigger's stored DDL against the generated one (reusing `normalise_ddl`,
so whitespace and `IF NOT EXISTS` spelling do not count as differences) and
drops the ones that differ, immediately before the create pass rebuilds them.

That generalises past this bug: it is the mechanism by which #101's outbox
payload changes (gaps S2/S3/S10 — `remind_at` and `sensitive` on the wire)
will reach existing databases at all. Dropping is safe for a trigger in a way
it would not be for a table — a trigger holds no rows — and SQLite offers no
`CREATE OR REPLACE TRIGGER`.

## Addendum, 2026-08-03: regenerated at v27, and the generator is now a script (issue #101)

The schema has been regenerated from `remind_me` v1.54.0 (`_SCHEMA_VERSION =
27`) and `SCHEMA_VERSION` bumped 19 → 27. Object-level diff, verified before
committing: **5 tables added** (`analytics_snapshots`, `memory_revisions`,
`reminder_deliveries`, `saved_searches`, `saved_search_seen_memories`), **6
indexes added**, **5 columns added** (`memories.remind_at`,
`memories.sensitive`, `sync_log.last_pull_at`/`.last_push_at`/
`.last_attempt_at`), the two `memories_outbox_*` payloads extended with
`remind_at`/`sensitive`, and **nothing removed**. The four graph outbox
triggers show as changed but differ only in line wrapping.

**The generation method is now `scripts/regenerate_schema.py`.** The original
ADR described the method in prose and performed it by hand, which meant the
next person to need it had to reconstruct an ad-hoc procedure from a document —
and the whole point of generating is to remove reconstruction steps. The script
stubs the reference's runtime-only dependencies (`httpx`, `numpy` via
`embeddings`) and, more importantly, installs a namespace stub for the
`remind_me_mcp` package root so that `__init__.py` — which imports the entire
MCP tool surface — never executes. That is what "import `db.py` standalone"
actually requires in practice, and it was the part most likely to be
rediscovered painfully. It also refuses to write anything if the generated
database's `user_version` does not match the reference's `_SCHEMA_VERSION`,
so a partial ladder cannot produce a mislabelled dump.

**The previous addendum's prediction held.** The `memories_outbox_au` guard
forward-ported by hand for issue #100 came back identical in the v27 dump, so
the annotated exception erased itself exactly as claimed rather than needing to
be reapplied. `schema_test.rs` now pins that
(`the_read_amplification_guard_survived_regeneration`) — the failure mode
worth guarding is a regeneration silently reverting a fix with no test
noticing, and "it will be fine because the reference has it too" is a claim, not
a check.

**On testing a generated artifact.** The pre-existing whole-schema tests
compare the live database against the shipped `schema_*.sql`, which catches
`migrations.rs` drifting from the SQL but not the SQL drifting from
`remind_me` — both sides would agree just as happily at v19. Asserting the dump
matches a checked-in copy of itself would be circular. So the new tests name
the v20–v27 objects explicitly, sourced from the reference's
`_migrate_vN_to_vN+1` function names rather than from the dump, which makes
regenerating against an older reference a test failure rather than a silent
downgrade.

**§5 of `ARCHITECTURE.md` no longer reproduces the DDL.** It had gone stale in
precisely the way this ADR exists to prevent — still showing
`last_accessed_at`, an `entities` with no `node_id`, cascading foreign keys on
`memory_entities`, and a `wiki_pages.topic` column, four shapes the schema
tests assert are wrong. A hand-maintained copy of a generated artifact is the
same failure as a hand-transcribed migration ladder, so it was replaced with
pointers to the generated files and the tests that police them.
