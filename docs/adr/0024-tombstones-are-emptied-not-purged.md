# ADR-0024: Tombstones are emptied, not purged

Status: Accepted (2026-09-29); implemented on the hub and the node
Date: 2026-09-29

## Context

A deleted memory stays behind as a tombstone: the row with `deleted_at` set
and `updated_at` bumped, so last-write-wins carries the delete to every
remote (ADR-0004). ADR-0004 and ADR-0007 left compacting tombstones as a
follow-up. ADR-0023 measured the cost on a real node: 47,008 of 62,060
memory rows were tombstones, each still holding its full text, loaded into
memory at every open. It ended with "purging them needs a rule for when
every remote has seen a delete".

What exists today (surveyed 2026-09-29):

- **Nodes never remove tombstones.** `TOMBSTONE_RETENTION_DAYS` appears only
  in comments. `sync_status`'s `compactable_now` counts tombstones older
  than the 30-day outbox retention; nothing acts on it.
- **The hub deletes them on request.** `POST /admin/compact_tombstones`
  hard-deletes memories whose `deleted_at` is older than
  `REMIND_ME_HUB_TOMBSTONE_RETENTION_DAYS` (default 90), plus their orphaned
  links, after saving the `hub_seq` floor (ADR-0021). Nothing schedules it.
- **Nobody knows who has seen a delete.** The hub keeps no list of nodes, no
  pull cursors and no acknowledgements; a pull cursor is only a request
  parameter. A node's own `sync_sends` and `sync_log` are local and pruned
  with the outbox after 30 days.
- **The hub's compaction can resurrect a deleted memory.** A node offline
  for longer than the retention never pulls the tombstone, so the memory
  stays live there. If that node later pushes the memory (a local edit, an
  unsent outbox row, or the backfill that turning sync on runs), the hub's
  `apply_memory` finds no row and inserts it live, whatever its
  `updated_at`, and every other node pulls it. The hub README calls this
  "an accepted gap"; nothing tests it.

## Decision

**A tombstone keeps what last-write-wins needs and drops the text.** No
tombstone row is ever deleted.

1. **What an emptied tombstone keeps:** `id`, `created_at`, `updated_at`,
   `accessed_at`, `deleted_at`, `category`, `source` and `metadata`.
   `content` becomes the fixed placeholder `TOMBSTONE_CONTENT`
   (`"(deleted)"`); `tags` becomes `[]`; `subject`, `predicate` and
   `object` become null. `source` and `metadata` stay because imports read
   them to recognise something already imported, and a re-import must not
   bring back a memory the user deleted.

2. **Why a placeholder and not an empty string.** A node's `upsert_record`
   rejects a record whose `content` is empty, and its pull cursor advances
   only past records that applied. An empty tombstone would stop every node
   on a current build at that record. The hub accepts an empty string, but
   the placeholder keeps the wire format valid for both, so no remote needs
   upgrading first.

3. **Why this needs no coordination.** Both sides decide a conflict on
   `updated_at` alone (`sync/record.rs`, the hub's `apply_memory`). An
   emptied tombstone wins and loses against exactly the records the full one
   would, so any node or the hub may empty a tombstone at any time without
   knowing who has seen it. A remote that is offline for a year still gets
   the delete, because the row is still there to pull.

4. **Where it happens:**
   - **At delete time on a node.** `delete_live` writes the emptied row, so
     the outbox payload queued for the delete carries no text either.
   - **Once, for existing tombstones on a node.** A local pass, run at open
     after the schema check, empties every tombstone that still has text.
     It is a storage change, not an edit: it leaves `updated_at` alone and
     queues nothing (`Origin::Sync`). Outbox rows already queued keep their
     snapshot until the outbox prunes them (30 days).
   - **On apply, on the hub and on nodes.** An incoming record that carries
     `deleted_at` and wins is stored emptied, whatever text it carried.
   - **On the hub, `/admin/compact_tombstones` empties instead of
     deleting.** The route keeps its name and retention setting so existing
     crons keep working; it now only reclaims text from tombstones that
     reached the hub before item 4's emptying on apply shipped.

5. **Revision history follows the delete.** Deleting a memory also deletes
   its `memory_revisions`, and the one-time pass deletes the revisions of
   every existing tombstone. Otherwise the text a delete removes stays
   readable in the history. The owner decided this on 2026-09-29.

## Consequences

- **The resurrection path closes.** The hub never loses a tombstone, so a
  stale push loses to it on `updated_at` like any other conflict.
- **Most of the space comes back, not all of it.** A tombstone shrinks to
  its ids, timestamps, `source` and `metadata`, a few hundred bytes. Rows
  still load at open. On the measured node that is about 47,000 small rows,
  an estimate of 10–15 MB, against the text they held; the before and after
  should be measured on that node, with `memory.engine`'s size and the
  daemon's RSS. The engine only returns the space once it rewrites the
  memories table, so the pass is followed by a compaction.
- **A delete is final.** The text of a deleted memory, and its earlier
  versions, are gone from every remote once each has emptied its copy. Nothing can restore a deleted
  memory today (`remind_me_revert` refuses one), so no feature is lost; an
  "undo delete" would have to be built on something else, such as a backup.
- **Tombstones are never purged.** Row count grows with every delete, at a
  few hundred bytes each. That is accepted until it matters.
- **The README's "accepted gap" note is replaced** by this ADR, and
  ADR-0004's and ADR-0007's pointers to "a future compaction pass" point
  here.

## Alternatives considered

- **Purge rows once every remote has seen the delete.** The hub would need a
  node registry, a stored pull cursor per node, a rule for nodes that stop
  syncing (a phone that is gone blocks the purge forever, unless it is
  dropped and must rebuild from scratch), and still a record of purged ids
  to refuse stale re-sends. That record is an emptied tombstone. It saves
  the last few hundred bytes per delete at the price of the only
  coordination state sync would have. Revisit if the row count itself
  becomes the problem.
- **Purge by time, as the hub does now.** Simple, and the source of the
  resurrection above.
- **Leave tombstones whole.** Correct, and it keeps paying for about three
  quarters of the node's memory rows in full.

## Open questions

- **Which `metadata` keys imports need.** Keeping all of `metadata` is the
  safe default. If it proves large for some sources, it can be narrowed to
  the keys the importers read, with a test for each importer.

## Amendment (2026-09-29): compaction ignores age

Decision 4 kept `/admin/compact_tombstones`'s retention setting. On the real
hub that meant it emptied nothing: every tombstone copied from Postgres was
between 30 and 90 days old, so none passed the 90-day default, and each kept
its text. Emptying never changes which write wins, so waiting protects
nothing. The route now empties every tombstone that still holds text, and
`REMIND_ME_HUB_TOMBSTONE_RETENTION_DAYS` is gone; a hub that still sets it
ignores it.

## Related

- ADR-0004 (sync protocol: deletes as soft deletes) and ADR-0007 (tombstones
  reach every node).
- ADR-0021 (the hub's `hub_seq` floor, which compaction keeps honouring).
- ADR-0023 (the tombstone count that prompted this).
