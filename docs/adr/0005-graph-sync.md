# ADR-0005: Knowledge-graph sync — entities, entity_relations, memory_entities

Status: Accepted
Date: 2026-07-30

## Context

`#57`'s own suggested split names the knowledge-graph tables as the second
slice, after the `memories`-only one ADR-0004 covers. This continues that
same PR/branch rather than starting a new one, since it builds directly on
the push/pull/echo-suppression machinery ADR-0004 already established —
splitting it into a separate PR would mean re-deriving that foundation on
an unmerged base.

Reading the reference (`sync.py`, `peer_server.py`, `db.py`) directly
before writing any of this settled the open questions:

- **Entities get their own, sync-specific conflict-resolution function**
  (`_upsert_entity_one`), distinct from the interactive `upsert_entity`
  used by direct tool calls (`remind_me_add`'s entity mentions,
  `remind_me_entity`). The interactive path's merge rule is "existing
  `kind` wins, only fills a hole"; the sync path's is straightforward LWW
  on `name`/`kind`/`node_id` (the incoming side simply overwrites on a
  win), with `aliases` *always* union-merging regardless of the winner —
  confirmed directly against the reference's own test suite
  (`test_upsert_entity_lww_newer_wins_aliases_union`,
  `test_upsert_entity_lww_loser_still_merges_aliases`). These are two
  different rules for two different call sites, not one relaxed into the
  other.
- **`entity_relations` and `memory_entities` links are immutable** —
  insert-or-ignore, no conflict resolution at all, matching their already
  content-derived/deterministic ids (`entity_relation_id`) or composite
  identity (`memory_id|entity_id`). Two nodes creating "the same" edge
  converge for free; there is nothing to reconcile.
- **No foreign key, by design, on either table** — a link or relation may
  reference a memory or entity that has not arrived on this node yet
  (sync delivers rows out of order). The row is inserted unconditionally
  and simply becomes visible the moment its referent shows up; nothing
  retries or reconciles it later. This was already this crate's existing
  schema shape before this PR — sync is *why* it looks that way, per
  `#57`'s own opening notes — this ADR just confirms the sync-receiving
  side actually relies on it rather than adding a check that would defeat
  the point.
- **Three additional pull endpoints, not one shared cursor** —
  `/sync/pull_entities`, `/sync/pull_links`, `/sync/pull_entity_relations`,
  each independently keyset-paged, and each with its own `sync_log` cursor
  row namespaced `"{remote_id}#entities"` / `"#links"` /
  `"#entity_relations"` — confirmed directly against `_pull_graph_table`'s
  shared drain loop and its `cursor_id = f"{remote_id}#{suffix}"`
  convention. `memory_entities` has no `updated_at` (immutable) and no
  single-column id, so its cursor pages on `(created_at,
  memory_id||'|'||entity_id)` instead — the same synthetic composite key
  already used as its wire `id`.
- **`exclude_node` is accepted but unused for links and relations** on the
  reference's own pull-serving side (`memory_entities` has no `node_id`
  column at all; `entity_relations` has one but the reference's own
  handler doesn't filter on it) — matched here exactly rather than "fixed,"
  for the same reason ADR-0002/ADR-0004 match other reference quirks
  exactly: this port tracks the reference's actual behavior, not an
  idealized version of it.
- **A push batch is heterogeneous.** Every graph-table trigger funnels into
  the same `sync_outbox` a `memories` row already uses, tagged with a
  `record_type` key (absent means `"memory"`, for backward compatibility)
  — so one `/sync/push` page can carry all four record shapes together,
  and the receiving side dispatches per-record on that tag
  (`_upsert_records`' `record_type` branch, ported as
  `graph::apply_incoming_record`).

## Decision

**New outbox triggers for `entities`/`entity_relations`/`memory_entities`,
initially installed by this crate's own code, not the generated schema** —
the schema dump this port started from predated the reference's own
entity-graph outbox triggers, so `sync::graph::ensure_schema` hand-rolled
`entities_outbox_ai`/`_au`, `entity_relations_outbox_ai`, and
`memory_entities_outbox_ai` to match what the reference's *source* showed,
the same way `#49`'s `vec_embeddings` table is this crate's own addition on
top of the generated schema. Un-gated at the time, matching this crate's
then-also-ungated `memories_outbox_*` triggers, tracked as the same
limitation in `#76`.

**Superseded by `#76`/ADR-0007**: regenerating the schema dump from the
reference's real, current schema code found that these four triggers are
themselves part of the *generated* schema (added alongside the entity
graph itself), each gated on `sync_flags.sync_enabled` like every other
outbox trigger. `sync::graph::ensure_schema` was removed entirely — the
triggers now live in `schema_triggers.sql`, gated, with no second,
hand-rolled copy to keep in sync with it.

**A dedicated sync-conflict function per record type**, not a reuse of the
interactive tool functions: `upsert_entity_record` (LWW +
always-union-merge aliases), `upsert_entity_relation_record` and
`upsert_link_record` (plain insert-or-ignore). Each does its own
echo-suppression, identical technique to `record::upsert_record`'s
(an outbox-id high-water-mark snapshotted before the write).

**`apply_incoming_record`** is the shared push-receiving dispatcher,
reading `record_type` (default `"memory"`) and routing to the right
upsert function, returning the wire id `/sync/push`'s response reports in
`processed_ids`.

**Three new pull functions and endpoints**, `pull_entities`/`pull_links`/
`pull_entity_relations`, sharing a generic paging helper
(`pull_graph_table`) parameterized by endpoint, cursor key, and which JSON
fields a raw record uses for cursor advancement — factored out from (but
not replacing) the already-tested, memories-specific `pull_remote`, to
avoid refactoring working code mid-flight for marginal gain. All three
tolerate a `404` (an older peer that predates graph sync) by stopping
without writing a cursor, matching the reference's own tolerance.

## A real bug this investigation caught before it shipped

Testing the heterogeneous-push-batch design surfaced a genuine matching
bug during development, not just a hypothetical: `push_outbox`'s
"which rows did the peer actually accept" check originally matched
`sync_outbox.memory_id` against `processed_ids`. That column holds the
*memory* half of a link's identity (`NEW.memory_id`, matching the
`memory_entities_outbox_ai` trigger) — but a link's *wire* id, and what a
receiving peer reports back in `processed_ids`, is the synthetic
`memory_id|entity_id` composite. Matching the two never succeeds, so every
link would have been silently marked "unsent" forever, retried every
cycle, with no visible symptom besides an outbox that never fully drains.
Fixed by matching against `payload["id"]` — the record's real wire id for
every one of the four record types — instead of the `sync_outbox.memory_id`
column, which was never the right thing to match on even for the original
memories-only slice (it happened to work there only because a memory's own
id and its outbox `memory_id` column are the same value). Caught by writing
a real push of all four record types together against a real peer server
in the same test, rather than testing each record type in isolation.

## Alternatives considered

**Reusing the interactive `upsert_entity`/`upsert_entity_relation`/
`link_memory_entity` functions directly for incoming sync records.**
Rejected: verified against the reference that it keeps these separate on
purpose (`_upsert_entity_one` is a distinct function from `_upsert_entity`)
because the two call sites have genuinely different merge semantics
("existing kind wins" for a direct tool call vs. straightforward LWW for a
synced record) — collapsing them would either break one caller's expected
behavior or require threading a mode flag through a function this port
would rather keep simple for its one real caller.

**One shared `/sync/pull` response carrying all four record types,
instead of four separate endpoints.** Rejected: the reference has four
separate endpoints with four separate cursors, and the mismatch between
`memories`/`entities`' `(updated_at, id)` keyset and `memory_entities`'
`(created_at, composite key)` keyset means a single unified query would
need its own bespoke merge logic for no interoperability benefit — a real
`remind_me` hub speaks the four-endpoint protocol, not an invented one.

## Consequences

- A vault with sync enabled now also carries entity/relation/link changes
  across nodes, not just memory content — the last thing gap `#12` (no FK
  on `memory_entities` "so sync can deliver rows out of order") was
  actually waiting on turning out to matter for something concrete.
- The same known limitations ADR-0004 already recorded still apply here:
  no `sync_flags` gating (tracked in `#76`), and OAuth/revocation/peer
  discovery remain unimplemented.
- `sync_log` now carries up to four cursor rows per remote instead of one
  — `"hub"`, `"hub#entities"`, `"hub#links"`, `"hub#entity_relations"` —
  which is the reference's own shape, not an invented complication.
