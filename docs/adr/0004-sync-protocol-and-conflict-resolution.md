# ADR-0004: Memories-only sync — one shared protocol, LWW with union/shallow-merge exceptions

Status: Accepted
Date: 2026-07-29

## Context

`#57` is an epic covering `remind_me_mcp/sync.py` (1221 lines), `peer_server.py`,
`oauth.py` (597 lines), and `remind_me_revoke_clients`. The issue is explicit
that this must be split into separately reviewable PRs, and suggests an
order: outbox drain and pull for `memories` only; then the knowledge-graph
tables; then peer discovery; then OAuth and revocation. This ADR covers only
the first slice.

Reading the reference (`sync.py`, `peer_server.py`, `config.py`, and the
generated `schema_triggers.sql`/`schema_tables.sql` already installed in
this crate) settled the open questions the issue's notes raised:

- **Hub and peer are genuinely the same protocol against different
  endpoints** — `peer_server.py` has no separate "hub mode"; `sync.py`'s
  `_push_outbox`/`_pull_remote` are the same two functions whether the
  remote is `HUB_URL` or a discovered peer. There is exactly one server
  implementation to port, not two.
- **`SYNC_ENABLED = bool(NODE_ID and HUB_URL and SYNC_SECRET)`** — a strict
  AND of all three, gating both this node's own background push/pull cycle
  and (via a separate, more surgical secret-only check in
  `peer_server.start_peer_server`) whether this node's own peer server
  binds a port at all.
- **`node_id`/`client` are stamped on every locally-created memory
  unconditionally**, not only when sync is enabled — `memory_add` passes
  `NODE_ID`/`CLIENT` straight from module-level config (`""`/`"unknown"`
  defaults) into every INSERT, regardless of `SYNC_ENABLED`. This is what
  lets a node that turns sync on *later* already know which of its existing
  memories were its own.
- **Conflict resolution** (`_upsert_one`): last-write-wins on `updated_at`,
  strict `>` — an equal timestamp means the incoming side loses, confirmed
  directly against the reference's own `test_upsert_equal_timestamp_is_noop`.
  `tags` always union-merge (dedup, order-preserving, local first) and
  `metadata` always shallow-merges (the LWW winner's value wins on a key
  collision, not recursively), **regardless of which side wins** — a losing
  record still contributes its tags/metadata, it just doesn't touch
  `updated_at` or any other column. A winning record on an *existing* row
  updates every column **except `created_at`**, which is insert-only.
- **Echo suppression**: applying a pulled record fires this crate's own
  outbox triggers, creating a new row for the very change just received.
  The reference immediately marks exactly that new row `sent_at = now`
  (via an outbox-id high-water-mark snapshotted before the write, scoped to
  this memory), so the next push cycle doesn't hand it back to the remote
  that sent it — while a genuinely concurrent local edit to the same memory
  is untouched.
- **Deletion has no separate wire operation** — a soft-delete (`deleted_at`
  set) rides the same INSERT/UPDATE trigger path as any other change; there
  is no hard `DELETE` propagation, because a `DELETE` produces no outbox row
  at all (the triggers only fire on INSERT/UPDATE) and would silently
  resurrect the memory on the next pull elsewhere.

## Decision

**Scope: `memories` only, hub sync only.** This slice implements exactly
what the issue's suggested first split names: draining the outbox to one
configured hub and pulling the hub's changes back, for the `memories`
table. Knowledge-graph table sync, Tailscale/static-peer discovery, OAuth,
and `remind_me_revoke_clients` are explicitly out of scope — each filed as
its own follow-up issue, the same way `#59` was already split out of this
epic for the outbox-growth defect.

**One protocol implementation, reused for both directions.** A single
`sync::server::serve_once` (this node accepting another's push/pull) and a
single pair of client functions, `sync::push_outbox`/`sync::pull_remote`
(this node pushing to / pulling from a configured hub) — matching the
reference's own "no separate hub mode" structure. There is currently no
peer discovery, so the client side only ever talks to one remote,
addressed by the constant `remote_id` `"hub"`.

**Conflict resolution ported exactly**, including the two subtleties most
likely to be gotten wrong: an equal `updated_at` is a loss for the incoming
side (not a no-op tie, not a win), and `created_at` is never touched on an
update — only set on a genuine insert. Echo suppression uses the same
outbox-id high-water-mark technique, reusing the `sync_outbox.sent_at`
column and `#59`'s existing `prune_outbox` (an echo-suppressed row is
already pruned on its very next opportunity — no new pruning rule needed).

**`node_id`/`client` are stamped on `add_memory` unconditionally**, exactly
matching the reference — not gated on `sync_enabled()`. `remind_me_update`
does **not** re-stamp them, also matching the reference exactly (verified
directly, not assumed).

**`delete_memory` tombstones instead of hard-deleting when `sync_enabled()`
is true**, exactly matching the reference's own conditional, including the
`updated_at` bump the tombstone needs to propagate through ordinary LWW.
Entity/feedback/association cleanup happens eagerly either way, since a
tombstoned memory is excluded from every read path regardless.

**A hand-rolled HTTP client and server**, matching this crate's established
pattern (`embedder.rs`'s Ollama client, `webhook.rs`'s ingest endpoint):
`std::net::TcpStream` for the client, a polling `TcpListener` for the
server, bearer auth via `SYNC_SECRET`. `http://` only — the same
simplifying choice `embedder.rs` already made; a deployment needing TLS
puts a reverse proxy in front.

**A background `SyncWorker`** runs push-then-pull-then-prune on
`REMIND_ME_SYNC_INTERVAL` (default 60s, matching the reference), started
once from the CLI's real `server`/`mcp` entry point — not from
`McpServer::new`, which the test suite also uses to build a server per
test, for the same reason `updater::start_background_check` isn't started
there either (`#58`'s ADR-0003 makes the identical argument).

## Resolved: tombstone propagation was schema-limited (`#76`, ADR-0007)

This ADR originally recorded a known limitation here: the generated
`schema_triggers.sql` this crate started from was dumped from an earlier
`remind_me` snapshot whose `memories_outbox_ai`/`_au` wrote a fixed
23-column payload ending at `superseded_by`, missing `deleted_at` (and
`doc_id`/`chunk_index`) — so `delete_memory`'s tombstone had no way to
propagate to another node, even though everything downstream of "the wire
payload carries what the trigger actually writes" was implemented
correctly.

Fixed in `#76` (see ADR-0007 for the full account): the reference's real
schema code was re-run directly (not hand-transcribed) to regenerate
`schema_tables.sql`/`schema_indexes.sql`/`schema_triggers.sql`, and
`SyncRecord`/`upsert_record` gained `deleted_at`, applied through the same
LWW path as every other column. A tombstone now propagates correctly.

**Re-embedding a synced memory is intentionally left to
`remind_me_reindex` (`#49`), not done inline here.** This slice's
`upsert_record` never calls the embedder — `#49` (embeddings) is a
separate, independently-unmerged epic slice, and coupling this PR to it
would create an artificial ordering dependency between two already-large
pieces of work. A memory that arrives via sync has no vector until the
next `remind_me_reindex`, exactly like a memory arriving via any bulk
import (`dbs`, MemPalace, chat/document) does today.

## Alternatives considered

**Building separate code paths for "acting as a hub" and "acting as a
peer."** Rejected: the reference doesn't have two, and inventing a
distinction the wire protocol doesn't need would be exactly backwards —
any node with a secret configured can serve either role.

**Gating the outbox triggers themselves on `sync_flags.sync_enabled`,
matching the reference's own reconciliation logic.** Rejected for this
slice: the installed, generated trigger SQL has no such gate at all (a
separate schema-generation-lag point from the tombstone one above), and
`#59` already solved the "grows without bound" consequence via retention
pruning rather than gating. Revisiting trigger-level gating is bundled with
the same future schema re-dump the tombstone limitation needs, not
something to hand-add to a generated, must-not-hand-edit file now.

**Deep-merging `metadata` instead of a shallow per-key merge.** Rejected:
the reference is explicitly, deliberately shallow (confirmed directly
against `test_merge_metadata_shallow_not_recursive`) — a nested
object/array on a shared key is replaced wholesale by the LWW winner's
value, not merged recursively. Matching this exactly, not "improving" it,
is the point.

## Consequences

- Turning sync on is three environment variables
  (`REMIND_ME_NODE_ID`/`REMIND_ME_HUB_URL`/`REMIND_ME_SYNC_SECRET`) plus a
  reachable hub speaking this same protocol — no new build-time dependency,
  no behavior change at all when unset.
- `remind_me_delete` on a sync-enabled node no longer immediately frees the
  row's storage — it is excluded from every read but persists until a
  future background compaction pass (the reference's own
  `TOMBSTONE_RETENTION_DAYS`) is ported, which is not yet implemented here
  either and is left for the same follow-up as the tombstone-propagation
  limitation above.
- A vault with sync enabled and no reachable hub degrades to exactly the
  same behavior as sync disabled for every read/write path except
  `delete_memory` (still tombstones — soft-deleting doesn't require the
  hub to be reachable) and the background worker (logs nothing anywhere
  yet, but records the failure on `SyncWorker::status()`, merged into
  `remind_me_server_status`).
- Knowledge-graph sync, peer discovery, OAuth, and `remind_me_revoke_clients`
  are unimplemented; each needs its own ADR when picked up, per the issue's
  own instruction to split this epic.
